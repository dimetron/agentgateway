use agent_core::bow::OwnedOrBorrowed;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cel::{ContextBuilder, Executor};
use crate::*;

#[derive(Clone, Debug)]
pub struct HTTPAuthorizationSet(RuleSets);

impl HTTPAuthorizationSet {
	pub fn new(rs: RuleSets) -> Self {
		Self(rs)
	}
	pub fn apply(&self, exec: &cel::Executor<'_>) -> anyhow::Result<()> {
		tracing::debug!("Checking HTTP request");
		let allowed = self
			.0
			.validate(|| Ok(agent_core::bow::OwnedOrBorrowed::Borrowed(exec)));
		if !allowed {
			anyhow::bail!("HTTP authorization denied");
		}
		Ok(())
	}

	pub fn register(&self, cel: &mut ContextBuilder) {
		self.0.register(cel);
	}
}

#[apply(schema!)]
pub struct RuleSet {
	#[serde(serialize_with = "se_policies", deserialize_with = "de_policies")]
	#[cfg_attr(feature = "schema", schemars(with = "Vec<String>"))]
	pub rules: PolicySet,
}

impl RuleSet {
	pub fn register(&self, cel: &mut ContextBuilder) {
		for rule in &self.rules.allow {
			cel.register_expression(rule.as_ref());
		}
		for rule in &self.rules.deny {
			cel.register_expression(rule.as_ref());
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct PolicySet {
	allow: Vec<Arc<cel::Expression>>,
	deny: Vec<Arc<cel::Expression>>,
}

#[derive(Clone, Debug)]
pub enum Policy {
	Allow(Arc<cel::Expression>),
	Deny(Arc<cel::Expression>),
}

#[apply(schema!)]
#[serde(untagged)]
enum RuleSerde {
	Object {
		#[serde(flatten)]
		rule: RuleTypeSerde,
	},
	PlainString(String),
}

#[apply(schema!)]
enum RuleTypeSerde {
	Allow(String),
	Deny(String),
}

impl PolicySet {
	pub fn add(&mut self, p: impl Into<String>) -> Result<(), cel::Error> {
		self.allow.push(Arc::new(cel::Expression::new(p)?));
		Ok(())
	}
}

pub fn se_policies<S: Serializer>(t: &PolicySet, serializer: S) -> Result<S::Ok, S::Error> {
	let mut m = serializer.serialize_map(Some(2))?;
	m.serialize_entry("allow", &t.allow)?;
	m.serialize_entry("deny", &t.deny)?;
	m.end()
}

pub fn de_policies<'de: 'a, 'a, D>(deserializer: D) -> Result<PolicySet, D::Error>
where
	D: Deserializer<'de>,
{
	let raw = Vec::<RuleSerde>::deserialize(deserializer)?;
	let mut res = PolicySet {
		allow: vec![],
		deny: vec![],
	};
	for r in raw {
		match r {
			RuleSerde::Object {
				rule: RuleTypeSerde::Allow(allow),
			}
			| RuleSerde::PlainString(allow) => res.allow.push(
				cel::Expression::new(&allow)
					.map(Arc::new)
					.map_err(|e| serde::de::Error::custom(e.to_string()))?,
			),
			RuleSerde::Object {
				rule: RuleTypeSerde::Deny(deny),
			} => res.deny.push(
				cel::Expression::new(deny)
					.map(Arc::new)
					.map_err(|e| serde::de::Error::custom(e.to_string()))?,
			),
		};
	}
	Ok(res)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuleSets(Vec<RuleSet>);

impl From<Vec<RuleSet>> for RuleSets {
	fn from(value: Vec<RuleSet>) -> Self {
		Self(value)
	}
}

impl RuleSets {
	pub fn register(&self, ctx: &mut ContextBuilder) {
		for rule_set in &self.0 {
			rule_set.register(ctx);
		}
	}
	pub fn validate<'a>(
		&self,
		exec: impl FnOnce() -> anyhow::Result<OwnedOrBorrowed<'a, Executor<'a>>>,
	) -> bool {
		let rule_sets = &self.0;
		let has_rules = rule_sets.iter().any(|r| r.has_rules());
		// If there are no rule sets, everyone has access
		if !has_rules {
			return true;
		}
		// Build executor only when we have rules
		let Ok(exec) = exec() else {
			return false;
		};
		let exec = exec.as_ref();
		// If there are any DENY, deny
		if rule_sets.iter().any(|r| r.denies(exec)) {
			return false;
		}
		// If there are any ALLOW, allow
		if rule_sets.iter().any(|r| r.allows(exec)) {
			return true;
		}
		// Else deny
		false
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

impl RuleSet {
	pub fn new(rules: PolicySet) -> Self {
		Self { rules }
	}

	pub fn has_rules(&self) -> bool {
		!self.rules.allow.is_empty() || !self.rules.deny.is_empty()
	}
	pub fn denies(&self, exec: &cel::Executor) -> bool {
		if self.rules.deny.is_empty() {
			false
		} else {
			self
				.rules
				.deny
				.iter()
				.any(|rule| exec.eval_bool(rule.as_ref()))
		}
	}

	pub fn allows(&self, exec: &cel::Executor) -> bool {
		if self.rules.allow.is_empty() {
			false
		} else {
			self
				.rules
				.allow
				.iter()
				.any(|rule| exec.eval_bool(rule.as_ref()))
		}
	}
}

#[cfg(any(test, feature = "internal_benches"))]
#[path = "authorization_tests.rs"]
mod tests;
