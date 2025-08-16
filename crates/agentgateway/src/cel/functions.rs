use std::collections::HashMap;
use std::string::ToString;
use std::sync::Arc;

use ::cel::extractors::{Identifier, This};
use ::cel::objects::{Key, Map, ValueType};
use ::cel::parser::Expression;
use ::cel::{Context, FunctionContext, ResolveResult, Value};
use once_cell::sync::Lazy;
use rand::random_range;

use crate::cel;
use crate::cel::to_value;

pub fn insert_all(ctx: &mut Context<'_>) {
	use super::strings;

	// Custom to agentgateway
	ctx.add_function("json", json_parse);
	ctx.add_function("to_json", to_json);
	ctx.add_function("with", with);
	ctx.add_function("flatten", flatten);
	ctx.add_function("flatten_recursive", flatten_recursive);
	ctx.add_function("map_values", map_values);
	ctx.add_function("variables", variables);
	ctx.add_function("random", || random_range(0.0..=1.0));

	// Using the go name, base64.encode is blocked by https://github.com/cel-rust/cel-rust/issues/103 (namespacing)
	ctx.add_function("base64_encode", base64_encode);
	ctx.add_function("base64_decode", base64_decode);

	// "Strings" extension
	// https://pkg.go.dev/github.com/google/cel-go/ext#Strings
	// TODO: add support for the newer versions
	ctx.add_function("charAt", strings::char_at);
	ctx.add_function("indexOf", strings::index_of);
	ctx.add_function("join", strings::join);
	ctx.add_function("lastIndexOf", strings::last_index_of);
	ctx.add_function("lowerAscii", strings::lower_ascii);
	ctx.add_function("upperAscii", strings::upper_ascii);
	ctx.add_function("trim", strings::trim);
	ctx.add_function("replace", strings::replace);
	ctx.add_function("split", strings::split);
	ctx.add_function("substring", strings::substring);
}

pub fn base64_encode(This(this): This<Arc<String>>) -> String {
	use base64::Engine;
	base64::prelude::BASE64_STANDARD.encode(this.as_bytes())
}

pub fn base64_decode(ftx: &FunctionContext, This(this): This<Arc<String>>) -> ResolveResult {
	use base64::Engine;
	base64::prelude::BASE64_STANDARD
		.decode(this.as_ref())
		.map(|v| Value::Bytes(Arc::new(v)))
		.map_err(|e| ftx.error(e))
}

fn with(
	ftx: &FunctionContext,
	This(this): This<Value>,
	ident: Identifier,
	expr: Expression,
) -> ResolveResult {
	let mut ptx = ftx.ptx.new_inner_scope();
	ptx.add_variable_from_value(&ident, this);
	ptx.resolve(&expr)
}

pub static FLATTEN_LIST: Lazy<Key> =
	Lazy::new(|| Key::String(Arc::new("$_meta_flatten_list".to_string())));
pub static FLATTEN_LIST_RECURSIVE: Lazy<Key> =
	Lazy::new(|| Key::String(Arc::new("$_meta_flatten_list_recursive".to_string())));
pub static FLATTEN_MAP: Lazy<Key> =
	Lazy::new(|| Key::String(Arc::new("$_meta_flatten_map".to_string())));
pub static FLATTEN_MAP_RECURSIVE: Lazy<Key> =
	Lazy::new(|| Key::String(Arc::new("$_meta_flatten_map_recursive".to_string())));

fn flatten(ftx: &FunctionContext, v: Value) -> ResolveResult {
	let res = match v {
		l @ Value::List(_) => Value::Map(Map {
			map: Arc::new(HashMap::from([(FLATTEN_LIST.clone(), l)])),
		}),
		m @ Value::Map(_) => Value::Map(Map {
			map: Arc::new(HashMap::from([(FLATTEN_MAP.clone(), m)])),
		}),
		_ => {
			return ftx.error("flatten only works on Map or List").into();
		},
	};
	res.into()
}

fn flatten_recursive(ftx: &FunctionContext, v: Value) -> ResolveResult {
	let res = match v {
		l @ Value::List(_) => Value::Map(Map {
			map: Arc::new(HashMap::from([(FLATTEN_LIST_RECURSIVE.clone(), l)])),
		}),
		m @ Value::Map(_) => Value::Map(Map {
			map: Arc::new(HashMap::from([(FLATTEN_MAP_RECURSIVE.clone(), m)])),
		}),
		_ => {
			return ftx.error("flatten only works on Map or List").into();
		},
	};
	res.into()
}

fn variables(ftx: &FunctionContext) -> ResolveResult {
	fn variables_inner<'context>(ctx: &'context Context<'context>) -> HashMap<cel::Key, Value> {
		match ctx {
			Context::Root { variables, .. } => variables
				.clone()
				.iter()
				.map(|(k, v)| (cel::Key::from(k.as_str()), v.clone()))
				.collect(),
			Context::Child {
				parent, variables, ..
			} => {
				let mut base = variables_inner(parent);
				base.extend(
					variables
						.iter()
						.map(|(k, v)| (cel::Key::from(k.as_str()), v.clone())),
				);
				base
			},
		}
	}
	Value::Map(Map {
		map: Arc::new(variables_inner(ftx.ptx)),
	})
	.into()
}
pub fn map_values(
	ftx: &FunctionContext,
	This(this): This<Value>,
	ident: Identifier,
	expr: Expression,
) -> ResolveResult {
	match this {
		Value::Map(map) => {
			let mut res = HashMap::with_capacity(map.map.len());
			let mut ptx = ftx.ptx.new_inner_scope();
			for (key, val) in map.map.as_ref() {
				ptx.add_variable_from_value(ident.clone(), val.clone());
				let value = ptx.resolve(&expr)?;
				res.insert(key.clone(), value);
			}
			Value::Map(Map { map: Arc::new(res) })
		},
		_ => return Err(this.error_expected_type(ValueType::Map)),
	}
	.into()
}

fn json_parse(ftx: &FunctionContext, v: Value) -> ResolveResult {
	let sv = match v {
		Value::String(b) => serde_json::from_str(b.as_str()),
		Value::Bytes(b) => serde_json::from_slice(b.as_ref()),
		_ => return Err(ftx.error("invalid type")),
	};
	let sv: serde_json::Value = sv.map_err(|e| ftx.error(e))?;
	to_value(sv).map_err(|e| ftx.error(e))
}

fn to_json(ftx: &FunctionContext, v: Value) -> ResolveResult {
	let pj = v.json().map_err(|e| ftx.error(e))?;
	Ok(Value::String(Arc::new(
		serde_json::to_string(&pj).map_err(|e| ftx.error(e))?,
	)))
}

#[cfg(any(test, feature = "internal_benches"))]
#[path = "functions_tests.rs"]
mod tests;
