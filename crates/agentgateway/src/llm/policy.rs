use std::ops::Deref;

use ::http::HeaderMap;
use async_openai::types::{ChatCompletionRequestMessage, CreateChatCompletionRequest};
use bytes::Bytes;

use crate::http::auth::{BackendAuth, SimpleBackendAuth};
use crate::http::jwt::Claims;
use crate::http::{Response, StatusCode, auth};
use crate::llm::policy::webhook::{MaskActionBody, Message, RequestAction};
use crate::llm::{AIError, pii, universal};
use crate::types::agent::Target;
use crate::{client, *};

#[apply(schema!)]
pub struct Policy {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	prompt_guard: Option<PromptGuard>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	defaults: Option<HashMap<String, serde_json::Value>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	overrides: Option<HashMap<String, serde_json::Value>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	prompts: Option<PromptEnrichment>,
}

#[apply(schema!)]
pub struct PromptEnrichment {
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	#[cfg_attr(
		feature = "schema",
		schemars(with = "crate::llm::SimpleChatCompletionMessage")
	)]
	append: Vec<ChatCompletionRequestMessage>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	#[cfg_attr(
		feature = "schema",
		schemars(with = "crate::llm::SimpleChatCompletionMessage")
	)]
	prepend: Vec<ChatCompletionRequestMessage>,
}

#[apply(schema!)]
pub struct PromptGuard {
	request: Option<PromptGuardRequest>,
}
impl Policy {
	pub fn apply_prompt_enrichment(
		&self,
		chat: &mut CreateChatCompletionRequest,
	) -> CreateChatCompletionRequest {
		if let Some(prompts) = &self.prompts {
			let old_messages = std::mem::take(&mut chat.messages);
			chat.messages = prompts
				.prepend
				.clone()
				.into_iter()
				.chain(old_messages)
				.chain(prompts.append.clone())
				.collect();
		}
		chat.clone()
	}
	pub fn unmarshal_request(&self, bytes: &Bytes) -> Result<CreateChatCompletionRequest, AIError> {
		if self.defaults.is_none() && self.overrides.is_none() && self.prompts.is_none() {
			// Fast path: directly bytes to typed
			return serde_json::from_slice(bytes.as_ref()).map_err(AIError::RequestParsing);
		}
		// Slow path: bytes --> json (transform) --> typed
		let v: serde_json::Value =
			serde_json::from_slice(bytes.as_ref()).map_err(AIError::RequestParsing)?;
		let serde_json::Value::Object(mut map) = v else {
			return Err(AIError::MissingField("request must be an object".into()));
		};
		for (k, v) in self.overrides.iter().flatten() {
			map.insert(k.clone(), v.clone());
		}
		for (k, v) in self.defaults.iter().flatten() {
			map.entry(k.clone()).or_insert_with(|| v.clone());
		}
		serde_json::from_value(serde_json::Value::Object(map)).map_err(AIError::RequestParsing)
	}
	pub async fn apply_prompt_guard(
		&self,
		client: client::Client,
		req: &mut universal::Request,
		http_headers: &HeaderMap,
		claims: Option<Claims>,
	) -> anyhow::Result<Option<Response>> {
		let Some(g) = self.prompt_guard.as_ref().and_then(|g| g.request.as_ref()) else {
			return Ok(None);
		};
		if let Some(moderation) = &g.openai_moderation {
			let model = moderation
				.model
				.clone()
				.unwrap_or(strng::literal!("omni-moderation-latest"));
			let auth = BackendAuth::from(moderation.auth.clone());
			let content = req
				.messages
				.iter()
				.filter_map(universal::message_text)
				.collect::<Vec<_>>();
			let mut rb = ::http::Request::builder()
				.uri("https://api.openai.com/v1/moderations")
				.method(::http::Method::POST)
				.header(::http::header::CONTENT_TYPE, "application/json");
			if let Some(claims) = claims {
				rb = rb.extension(claims);
			}
			let mut req = rb.body(http::Body::from(serde_json::to_vec(&serde_json::json!({
				"input": content,
				"model": model,
			}))?))?;
			auth::apply_backend_auth(Some(&auth), &mut req).await?;
			let resp = client.simple_call(req).await;
			let resp: async_openai::types::CreateModerationResponse =
				json::from_body(resp?.into_body()).await?;
			if resp.results.iter().any(|r| r.flagged) {
				return Ok(Some(g.response.as_response()));
			}
		}
		if let Some(webhook) = &g.webhook {
			let whr = webhook::send_request(client.clone(), &webhook.target, http_headers, req).await?;
			match whr.action {
				RequestAction::Mask(mask) => {
					debug!(
						"webhook masked: {}",
						mask
							.reason
							.unwrap_or_else(|| "no reason specified".to_string())
					);
					let MaskActionBody::PromptMessages(body) = mask.body else {
						anyhow::bail!("invalid webhook response");
					};
					let msgs = body.messages;
					req.messages = msgs.into_iter().map(Self::convert_message).collect();
				},
				RequestAction::Reject(rej) => {
					debug!(
						"webhook rejected: {}",
						rej
							.reason
							.unwrap_or_else(|| "no reason specified".to_string())
					);
					return Ok(Some(
						::http::response::Builder::new()
							.status(rej.status_code)
							.body(http::Body::from(rej.body))?,
					));
				},
				RequestAction::Pass(pass) => {
					debug!(
						"webhook passed: {}",
						pass
							.reason
							.unwrap_or_else(|| "no reason specified".to_string())
					);
					// No action needed
				},
			}
		}
		for msg in &mut req.messages {
			let content = {
				let Some(content) = universal::message_text(msg) else {
					continue;
				};
				content.to_string()
			};
			if let Some(rgx) = &g.regex {
				for r in &rgx.rules {
					match r {
						RegexRule::Builtin { builtin } => match builtin {
							Builtin::Ssn => pii::recognizer(pii::SSN.deref(), &content),
							Builtin::CreditCard => pii::recognizer(pii::CC.deref(), &content),
							Builtin::PhoneNumber => pii::recognizer(pii::PHONE.deref(), &content),
							Builtin::Email => pii::recognizer(pii::EMAIL.deref(), &content),
						},
						RegexRule::Regex { pattern, name } => {
							if let Some(m) = pattern.find(&content) {
								if let Action::Reject { response } = &rgx.action {
									return Ok(Some(response.as_response()));
								}
								let mut new_content = content.to_string();
								new_content.replace_range(m.range(), &format!("<{name}>"));
								*msg = Self::convert_message(Message {
									role: universal::message_role(msg).to_string(),
									content: new_content,
								});
							}
						},
					}
				}
			}
		}
		Ok(None)
	}

	fn convert_message(r: Message) -> ChatCompletionRequestMessage {
		match r.role.as_str() {
			"system" => universal::RequestMessage::from(universal::RequestSystemMessage::from(r.content)),
			"assistant" => {
				universal::RequestMessage::from(universal::RequestAssistantMessage::from(r.content))
			},
			// TODO: the webhook API cannot express functions or tools...
			"function" => universal::RequestMessage::from(universal::RequestFunctionMessage {
				content: Some(r.content),
				name: "".to_string(),
			}),
			"tool" => universal::RequestMessage::from(universal::RequestToolMessage {
				content: universal::RequestToolMessageContent::from(r.content),
				tool_call_id: "".to_string(),
			}),
			_ => universal::RequestMessage::from(universal::RequestUserMessage::from(r.content)),
		}
	}
}

#[apply(schema!)]
pub struct PromptGuardRequest {
	#[serde(default)]
	response: PromptGuardResponse,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	regex: Option<RegexRules>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	webhook: Option<Webhook>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	openai_moderation: Option<Moderation>,
}

#[apply(schema!)]
pub struct RegexRules {
	#[serde(default)]
	action: Action,
	rules: Vec<RegexRule>,
}

#[apply(schema!)]
#[serde(untagged)]
pub enum RegexRule {
	Builtin {
		builtin: Builtin,
	},
	Regex {
		#[serde(with = "serde_regex")]
		#[cfg_attr(feature = "schema", schemars(with = "String"))]
		pattern: regex::Regex,
		name: String,
	},
}

impl PromptGuardResponse {
	pub fn as_response(&self) -> Response {
		::http::response::Builder::new()
			.status(self.status)
			.body(http::Body::from(self.body.clone()))
			.expect("static request should succeed")
	}
}

#[apply(schema!)]
pub enum Builtin {
	#[serde(rename = "ssn")]
	Ssn,
	CreditCard,
	PhoneNumber,
	Email,
}

#[apply(schema!)]
pub struct Rule<T> {
	action: Action,
	rule: T,
}

#[apply(schema!)]
pub struct NamedRegex {
	#[serde(with = "serde_regex")]
	#[cfg_attr(feature = "schema", schemars(with = "String"))]
	pattern: regex::Regex,
	name: String,
}

#[apply(schema!)]
pub struct Webhook {
	target: Target,
	// TODO: headers
}

#[apply(schema!)]
pub struct Moderation {
	/// Model to use. Defaults to `omni-moderation-latest`
	#[serde(default, skip_serializing_if = "Option::is_none")]
	model: Option<Strng>,
	auth: SimpleBackendAuth,
}

#[apply(schema!)]
#[derive(Default)]
pub enum Action {
	#[default]
	Mask,
	Reject {
		#[serde(default)]
		response: PromptGuardResponse,
	},
}

#[apply(schema!)]
pub struct PromptGuardResponse {
	#[serde(default = "default_body", serialize_with = "ser_string_or_bytes")]
	body: Bytes,
	#[serde(default = "default_code", with = "http_serde::status_code")]
	#[cfg_attr(feature = "schema", schemars(with = "std::num::NonZeroU16"))]
	status: StatusCode,
}

impl Default for PromptGuardResponse {
	fn default() -> Self {
		Self {
			body: default_body(),
			status: default_code(),
		}
	}
}

#[apply(schema!)]
pub struct PromptGuardRegex {}
fn default_code() -> StatusCode {
	StatusCode::FORBIDDEN
}

fn default_body() -> Bytes {
	Bytes::from_static(b"The request was rejected due to inappropriate content")
}

mod webhook {
	use ::http::header::CONTENT_TYPE;
	use ::http::{HeaderMap, HeaderValue, header};
	use serde::{Deserialize, Serialize};

	use crate::client::Client;
	use crate::llm::universal::Request;
	use crate::types::agent::Target;
	use crate::*;

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsPromptRequest {
		/// body contains the object which is a list of the Message JSON objects from the prompts in the request
		pub body: PromptMessages,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsPromptResponse {
		/// action is the action to be taken based on the request.
		/// The following actions are available on the response:
		/// - PassAction: No action is required.
		/// - MaskAction: Mask the response body.
		/// - RejectAction: Reject the request.
		pub action: RequestAction,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsResponseRequest {
		/// body contains the object with a list of Choice that contains the response content from the LLM.
		pub body: ResponseChoices,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct GuardrailsResponseResponse {
		/// action is the action to be taken based on the request.
		/// The following actions are available on the response:
		/// - PassAction: No action is required.
		/// - MaskAction: Mask the response body.
		pub action: ResponseAction,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct Message {
		/// The role associated to the content in this message.
		pub role: String,
		/// The content text for this message.
		pub content: String,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct PromptMessages {
		/// List of prompt messages including role and content.
		pub messages: Vec<Message>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct ResponseChoice {
		/// message contains the role and text content of the response from the LLM model.
		pub message: Message,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct ResponseChoices {
		/// list of possible independent responses from the LLM
		pub choices: Vec<ResponseChoice>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct PassAction {
		/// reason is a human readable string that explains the reason for the action.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub reason: Option<String>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct MaskAction {
		/// body contains the modified messages that masked out some of the original contents.
		/// When used in a GuardrailPromptResponse, this should be PromptMessages.
		/// When used in GuardrailResponseResponse, this should be ResponseChoices
		pub body: MaskActionBody,
		/// reason is a human readable string that explains the reason for the action.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub reason: Option<String>,
	}

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "snake_case")]
	pub struct RejectAction {
		/// body is the rejection message that will be used for HTTP error response body.
		pub body: String,
		/// status_code is the HTTP status code to be returned in the HTTP error response.
		pub status_code: u16,
		/// reason is a human readable string that explains the reason for the action.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub reason: Option<String>,
	}

	/// Enum for actions available in prompt responses
	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(untagged, rename_all = "snake_case")]
	pub enum RequestAction {
		Mask(MaskAction),
		Reject(RejectAction),
		Pass(PassAction),
	}

	/// Enum for actions available in response responses
	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(untagged, rename_all = "snake_case")]
	pub enum ResponseAction {
		Mask(MaskAction),
		Pass(PassAction),
	}

	/// Enum for MaskAction body that can be either PromptMessages or ResponseChoices
	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(untagged)]
	pub enum MaskActionBody {
		PromptMessages(PromptMessages),
		ResponseChoices(ResponseChoices),
	}

	fn build_request_for_request(
		target: &Target,
		http_headers: &HeaderMap,
		i: &Request,
	) -> anyhow::Result<crate::http::Request> {
		let body = GuardrailsPromptRequest {
			body: PromptMessages {
				messages: i
					.messages
					.iter()
					.filter_map(|m| {
						let role = llm::universal::message_role(m).to_string();
						let content = llm::universal::message_text(m).map(|s| s.to_string());
						content.map(|content| Message { role, content })
					})
					.collect(),
			},
		};
		let body_bytes = serde_json::to_vec(&body)?;
		let mut rb = ::http::Request::builder()
			.uri(format!("http://{target}/request"))
			.method(http::Method::POST);
		for (k, v) in http_headers {
			// TODO: this is configurable by users
			if k == header::CONTENT_LENGTH {
				// TODO: probably others
				continue;
			}
			rb = rb.header(k.clone(), v.clone());
		}
		let req = rb
			.header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
			.body(crate::http::Body::from(body_bytes))?;
		Ok(req)
	}

	pub async fn send_request(
		client: Client,
		target: &Target,
		http_headers: &HeaderMap,
		req: &Request,
	) -> anyhow::Result<GuardrailsPromptResponse> {
		let whr = build_request_for_request(target, http_headers, req)?;
		let res = client
			.call(client::Call {
				req: whr,
				target: target.clone(),
				transport: Default::default(), // TODO: use policies
			})
			.await?;
		let bb = axum::body::to_bytes(res.into_body(), 2_097_152).await?;
		let parsed = serde_json::from_slice::<GuardrailsPromptResponse>(&bb)?;
		Ok(parsed)
	}
}
