//! What changed between two pictures, as the page applies it.
//!
//! The page holds the picture it was last sent and changes only the parts
//! that differ, so what the owner is reading, typing or dragging is left
//! alone while the rest moves. A change is one of:
//!
//! - `["s", path, value]`: set what is at `path` (a new key of an object,
//!   or one index past the end of a list to add to it);
//! - `["d", path]`: remove a key of an object;
//! - `["t", path, length]`: shorten a list to `length`.
//!
//! A path is a list of keys and indexes from the top of the picture. An
//! empty path is the whole picture.
//!
//! Lists are compared index by index. An object at an index that has
//! become a different thing (its `key`, `id`, `unit`, `bead`, `ts` or
//! `name` differs) is set whole, so the page never turns one goal into
//! another field by field.

use serde_json::{Map, Value, json};

/// The fields that say which thing an object in a list is, the first one
/// either side has deciding.
const IDENTITY: [&str; 6] = ["key", "id", "unit", "bead", "ts", "name"];

/// The changes that turn `old` into `new`.
pub fn diff(old: &Value, new: &Value) -> Vec<Value> {
    let mut changes = Vec::new();
    let mut path = Vec::new();
    walk(old, new, &mut path, &mut changes);
    changes
}

fn walk(old: &Value, new: &Value, path: &mut Vec<Value>, changes: &mut Vec<Value>) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => objects(old, new, path, changes),
        (Value::Array(old), Value::Array(new)) => lists(old, new, path, changes),
        _ if old == new => {}
        _ => changes.push(json!(["s", path, new])),
    }
}

fn objects(
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    path: &mut Vec<Value>,
    changes: &mut Vec<Value>,
) {
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        path.push(Value::String(key.clone()));
        changes.push(json!(["d", path]));
        path.pop();
    }
    for (key, value) in new {
        path.push(Value::String(key.clone()));
        match old.get(key) {
            Some(before) => walk(before, value, path, changes),
            None => changes.push(json!(["s", path, value])),
        }
        path.pop();
    }
}

fn lists(old: &[Value], new: &[Value], path: &mut Vec<Value>, changes: &mut Vec<Value>) {
    for (index, value) in new.iter().enumerate() {
        path.push(Value::from(index));
        match old.get(index) {
            Some(before) if same_thing(before, value) => walk(before, value, path, changes),
            Some(before) if before == value => {}
            _ => changes.push(json!(["s", path, value])),
        }
        path.pop();
    }
    if new.len() < old.len() {
        changes.push(json!(["t", path, new.len()]));
    }
}

/// Whether two values at one index of a list are the same thing, so that
/// one may be changed into the other field by field.
fn same_thing(old: &Value, new: &Value) -> bool {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => IDENTITY
            .iter()
            .find(|field| old.contains_key(**field) || new.contains_key(**field))
            .is_none_or(|field| old.get(*field) == new.get(*field)),
        (Value::Array(_), Value::Array(_)) => true,
        _ => false,
    }
}

/// Apply changes as the page does (`web/src/lib/live.svelte.ts`), to
/// prove that they say what `diff` meant. None if one of them does not
/// fit what it is applied to.
pub fn apply(mut on: Value, changes: &[Value]) -> Option<Value> {
    for change in changes {
        let op = change.get(0)?.as_str()?;
        let path = change.get(1)?.as_array()?;
        let Some((last, parents)) = path.split_last() else {
            if op != "s" {
                return None;
            }
            on = change.get(2)?.clone();
            continue;
        };
        let mut at = &mut on;
        for step in parents {
            at = match step {
                Value::String(key) => at.get_mut(key.as_str())?,
                Value::Number(index) => at.get_mut(usize::try_from(index.as_u64()?).ok()?)?,
                _ => return None,
            };
        }
        match (op, last, at) {
            ("s", Value::String(key), Value::Object(map)) => {
                map.insert(key.clone(), change.get(2)?.clone());
            }
            ("s", Value::Number(index), Value::Array(list)) => {
                let index = usize::try_from(index.as_u64()?).ok()?;
                let value = change.get(2)?.clone();
                match index.cmp(&list.len()) {
                    std::cmp::Ordering::Less => list[index] = value,
                    std::cmp::Ordering::Equal => list.push(value),
                    std::cmp::Ordering::Greater => return None,
                }
            }
            ("d", Value::String(key), Value::Object(map)) => {
                map.remove(key.as_str())?;
            }
            ("t", Value::String(key), Value::Object(map)) => {
                let length = usize::try_from(change.get(2)?.as_u64()?).ok()?;
                map.get_mut(key.as_str())?.as_array_mut()?.truncate(length);
            }
            ("t", Value::Number(index), Value::Array(list)) => {
                let length = usize::try_from(change.get(2)?.as_u64()?).ok()?;
                list.get_mut(usize::try_from(index.as_u64()?).ok()?)?
                    .as_array_mut()?
                    .truncate(length);
            }
            _ => return None,
        }
    }
    Some(on)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(old: Value, new: Value) -> Vec<Value> {
        let changes = diff(&old, &new);
        assert_eq!(apply(old, &changes), Some(new));
        changes
    }

    #[test]
    fn nothing_changed_is_no_change() {
        let picture = json!({"projects": [{"id": "toy", "goals": [{"id": "g1", "cost": 2.0}]}], "release": null});
        assert!(diff(&picture, &picture).is_empty());
    }

    #[test]
    fn only_what_changed_is_sent() {
        let changes = round_trip(
            json!({"projects": [{"id": "toy", "goals": [{"id": "g1", "cost": 2.0, "wakes": 3}]}]}),
            json!({"projects": [{"id": "toy", "goals": [{"id": "g1", "cost": 2.5, "wakes": 3}]}]}),
        );
        assert_eq!(
            changes,
            [json!(["s", ["projects", 0, "goals", 0, "cost"], 2.5])]
        );
    }

    #[test]
    fn keys_come_and_go() {
        let changes = round_trip(
            json!({"a": 1, "run": {"step": "build"}}),
            json!({"a": 1, "parked": "why"}),
        );
        assert_eq!(
            changes,
            [json!(["d", ["run"]]), json!(["s", ["parked"], "why"])]
        );
    }

    #[test]
    fn lists_grow_and_shrink() {
        round_trip(json!({"runs": [1, 2]}), json!({"runs": [1, 2, 3, 4]}));
        let changes = round_trip(json!({"runs": [1, 2, 3]}), json!({"runs": [1]}));
        assert_eq!(changes, [json!(["t", ["runs"], 1])]);
        round_trip(json!([[1, 2], [3]]), json!([[1], [3, 4]]));
    }

    #[test]
    fn a_different_thing_at_an_index_is_set_whole() {
        let changes = round_trip(
            json!({"inbox": [{"key": "a", "title": "One", "at": 1}]}),
            json!({"inbox": [{"key": "b", "title": "Two", "at": 1}, {"key": "a", "title": "One", "at": 1}]}),
        );
        assert_eq!(
            changes[0],
            json!(["s", ["inbox", 0], {"key": "b", "title": "Two", "at": 1}])
        );
    }

    #[test]
    fn a_change_of_kind_is_set_whole() {
        round_trip(json!({"x": [1]}), json!({"x": {"a": 1}}));
        round_trip(json!({"x": null}), json!({"x": [1]}));
        round_trip(json!([1]), json!({"a": 1}));
    }
}
