use anyhow::{Result, anyhow};
use jaq_core::{
    Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use serde_json::Value;

/// Compiled query reused for one operation; compilation performs no source I/O.
pub enum Query {
    Identity,
    Jq(jaq_core::Filter<data::JustLut<jaq_json::Val>>),
    Jmespath(jmespath::Expression<'static>),
}

impl Query {
    pub fn compile(jq: Option<&str>, jmes: Option<&str>) -> Result<Self> {
        if let Some(code) = jq {
            return Ok(Self::Jq(compile_jq(code)?));
        }
        if let Some(code) = jmes {
            return Ok(Self::Jmespath(
                jmespath::compile(code).map_err(|e| anyhow!("invalid JMESPath expression: {e}"))?,
            ));
        }
        Ok(Self::Identity)
    }
    pub fn run(&self, input: Value) -> Result<Vec<Value>> {
        match self {
            Self::Identity => Ok(vec![input]),
            Self::Jq(filter) => run_jq(filter, input),
            Self::Jmespath(expr) => Ok(vec![serde_json::to_value(
                expr.search(input)
                    .map_err(|e| anyhow!("JMESPath evaluation failed: {e}"))?,
            )?]),
        }
    }
}

fn compile_jq(code: &str) -> Result<jaq_core::Filter<data::JustLut<jaq_json::Val>>> {
    let arena = Arena::default();
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let loader = Loader::new(defs);
    let modules = loader
        .load(&arena, File { code, path: () })
        .map_err(|e| anyhow!("invalid jq expression: {e:?}"))?;
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());
    let filter = jaq_core::Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|e| anyhow!("cannot compile jq expression: {e:?}"))?;
    Ok(filter)
}

fn run_jq(
    filter: &jaq_core::Filter<data::JustLut<jaq_json::Val>>,
    input: Value,
) -> Result<Vec<Value>> {
    let ctx = Ctx::<data::JustLut<jaq_json::Val>>::new(&filter.lut, Vars::new([]));
    let input: jaq_json::Val = serde_json::from_value(input)?;
    filter
        .id
        .run((ctx, input))
        .map(unwrap_valr)
        .map(|result| {
            let value = result.map_err(|e| anyhow!("jq evaluation failed: {e}"))?;
            Ok(serde_json::from_str(&value.to_string())?)
        })
        .collect()
}

/// Execute a jq expression without filesystem modules or I/O functions.
pub fn jq(input: Value, code: &str) -> Result<Vec<Value>> {
    Query::compile(Some(code), None)?.run(input)
}

pub fn jmespath(input: Value, code: &str) -> Result<Value> {
    Ok(Query::compile(None, Some(code))?.run(input)?.remove(0))
}

pub fn project(input: &mut Value, fields: &[String]) {
    if let Some(items) = input.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            let mut selected = serde_json::Map::new();
            for field in fields {
                let value = field.split('.').try_fold(&*item, |v, key| v.get(key));
                selected.insert(field.clone(), value.cloned().unwrap_or(Value::Null));
            }
            *item = Value::Object(selected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn common_agent_queries() {
        let data = json!({"items":[{"title":"a","metrics":{"comments":4}}, {"title":"b","metrics":{"comments":60}}]});
        assert_eq!(
            jq(
                data.clone(),
                ".items[] | select(.metrics.comments >= 50) | {title}"
            )
            .unwrap(),
            vec![json!({"title":"b"})]
        );
        assert_eq!(
            jmespath(data, "items[?metrics.comments >= `50`].title").unwrap(),
            json!(["b"])
        );
        assert_eq!(
            jq(json!([3, 1, 3, 2]), "unique | sort | reverse").unwrap(),
            vec![json!([3, 2, 1])]
        );
    }
    #[test]
    fn query_errors_are_not_empty_success() {
        assert!(jq(json!(null), "invalid(").is_err());
        assert!(jq(json!(3), ".[]").is_err());
        assert!(jmespath(json!(null), "[?").is_err());
    }
}
