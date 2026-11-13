//! `_bulk` NDJSON: parsed line by line under fixed byte and item bounds, with a
//! per-item verdict and no commit until the whole request is structurally valid.
use std::io::{BufRead, BufReader, Read};

use serde_json::Value;

use crate::ApiError;

/// Largest bulk body, bytes (the profile's example limit).
pub const MAX_BULK_BYTES: u64 = 10 * 1024 * 1024;
/// Most actions in one bulk.
pub const MAX_BULK_ITEMS: usize = 5000;

#[derive(Debug)]
pub(crate) enum Action {
    Index,
    Delete,
}

#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) action: String,
    pub(crate) index: String,
    pub(crate) id: String,
    /// The action to run, the source it carries, or the reason it was refused.
    pub(crate) work: Result<(Action, Option<Value>), ApiError>,
}

fn too_big(what: &str) -> ApiError {
    ApiError::new(
        413,
        "payload_too_large_exception",
        format!("bulk {what} limit exceeded"),
    )
}

/// Read one line without ever holding more than the remaining byte budget.
fn line<R: BufRead>(reader: &mut R, budget: &mut u64) -> Result<Option<Vec<u8>>, ApiError> {
    let mut buf = Vec::new();
    let mut limited = reader.by_ref().take(*budget + 1);
    let n = limited
        .read_until(b'\n', &mut buf)
        .map_err(|_| ApiError::bad("the request body could not be read"))?;
    if n == 0 {
        return Ok(None);
    }
    if n as u64 > *budget {
        return Err(too_big("byte"));
    }
    *budget -= n as u64;
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(Some(buf))
}

/// Parse a whole bulk body. A malformed action line refuses the request: the
/// stream cannot be resynchronised. A bad source line is that item's error.
pub(crate) fn parse<R: Read>(body: R, default_index: Option<&str>) -> Result<Vec<Item>, ApiError> {
    let mut reader = BufReader::new(body);
    let mut budget = MAX_BULK_BYTES;
    let mut items = Vec::new();
    while let Some(text) = line(&mut reader, &mut budget)? {
        if text.is_empty() {
            continue;
        }
        if items.len() == MAX_BULK_ITEMS {
            return Err(too_big("item"));
        }
        let action: Value = serde_json::from_slice(&text)
            .map_err(|_| ApiError::parse("malformed bulk action line"))?;
        let object = action
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or_else(|| ApiError::parse("an action line names exactly one action"))?;
        let (name, meta) = object.iter().next().expect("one entry");
        let meta = meta
            .as_object()
            .ok_or_else(|| ApiError::parse("action metadata must be an object"))?;
        if let Some(extra) = meta.keys().find(|k| *k != "_index" && *k != "_id") {
            return Err(ApiError::parse(format!(
                "unsupported action parameter [{extra}]"
            )));
        }
        let index = match (meta.get("_index"), default_index) {
            (Some(Value::String(i)), _) => i.clone(),
            (None, Some(i)) => i.to_string(),
            _ => return Err(ApiError::bad("an action needs an _index")),
        };
        let id = match meta.get("_id") {
            Some(Value::String(i)) if !i.is_empty() => i.clone(),
            _ => String::new(),
        };
        let needs_source = matches!(name.as_str(), "index" | "create" | "update");
        let source = if needs_source {
            Some(
                line(&mut reader, &mut budget)?
                    .ok_or_else(|| ApiError::parse("an action is missing its source line"))?,
            )
        } else {
            None
        };
        let work = if id.is_empty() {
            Err(ApiError::bad("an explicit string _id is required"))
        } else {
            match (name.as_str(), source) {
                ("index", Some(source)) => serde_json::from_slice::<Value>(&source)
                    .map(|v| (Action::Index, Some(v)))
                    .map_err(|_| {
                        ApiError::new(400, "mapper_parsing_exception", "malformed source line")
                    }),
                ("delete", _) => Ok((Action::Delete, None)),
                ("create" | "update", _) => Err(ApiError::new(
                    400,
                    "unsupported_exception",
                    format!("the [{name}] action is not supported by this node"),
                )),
                _ => return Err(ApiError::parse(format!("unknown bulk action [{name}]"))),
            }
        };
        items.push(Item {
            action: name.clone(),
            index,
            id,
            work,
        });
    }
    if items.is_empty() {
        return Err(ApiError::bad("the bulk body has no actions"));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(body: &str) -> Result<Vec<Item>, ApiError> {
        parse(body.as_bytes(), Some("d"))
    }

    #[test]
    fn actions_parse_in_order_with_per_item_verdicts() {
        let items = run("{\"index\":{\"_id\":\"1\"}}\n{\"a\":1}\n{\"delete\":{\"_index\":\"o\",\"_id\":\"2\"}}\r\n\n{\"create\":{\"_id\":\"3\"}}\n{}\n{\"index\":{\"_id\":\"4\"}}\nnot json\n{\"index\":{}}\n{}\n").unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!((items[0].index.as_str(), items[0].id.as_str()), ("d", "1"));
        assert!(matches!(items[0].work, Ok((Action::Index, Some(_)))));
        assert_eq!(items[1].index, "o");
        assert!(matches!(items[1].work, Ok((Action::Delete, None))));
        assert_eq!(
            items[2].work.as_ref().unwrap_err().kind,
            "unsupported_exception"
        );
        assert_eq!(
            items[3].work.as_ref().unwrap_err().kind,
            "mapper_parsing_exception"
        );
        assert_eq!(
            items[4].work.as_ref().unwrap_err().kind,
            "illegal_argument_exception"
        );
    }

    #[test]
    fn a_broken_stream_refuses_the_whole_request() {
        for body in [
            "",
            "\n\n",
            "nope\n",
            "[]\n",
            "{\"index\":{},\"delete\":{}}\n",
            "{\"index\":5}\n",
            "{\"index\":{\"routing\":\"r\",\"_id\":\"1\"}}\n{}\n",
            "{\"index\":{\"_id\":\"1\"}}\n",
            "{\"bogus\":{\"_id\":\"1\"}}\n",
        ] {
            assert!(run(body).is_err(), "{body:?}");
        }
        assert!(parse("{\"delete\":{\"_id\":\"1\"}}\n".as_bytes(), None).is_err());
    }

    #[test]
    fn byte_and_item_limits_are_one_over_refused() {
        let item = "{\"delete\":{\"_id\":\"1\"}}\n";
        let ok = item.repeat(MAX_BULK_ITEMS);
        assert_eq!(run(&ok).unwrap().len(), MAX_BULK_ITEMS);
        let over = item.repeat(MAX_BULK_ITEMS + 1);
        assert_eq!(run(&over).unwrap_err().status, 413);
        let mut huge = String::from("{\"index\":{\"_id\":\"1\"}}\n{\"a\":\"");
        huge.push_str(&"x".repeat(MAX_BULK_BYTES as usize));
        huge.push_str("\"}\n");
        assert_eq!(run(&huge).unwrap_err().status, 413);
    }
}
