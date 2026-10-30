use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

pub const VALID_STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];
pub const MAX_TODO_CONTENT_CHARS: usize = 4000;
pub const MAX_TODO_ITEMS: usize = 256;
pub const MAX_TODO_RESULT_CHARS: usize = 512_000;
pub const TRUNCATION_MARKER: &str = "… [truncated]";
pub const TODO_INJECTION_HEADER: &str =
    "[Your active task list was preserved across context compression]";

pub fn status_marker(status: &str) -> &'static str {
    match status {
        "completed" => "[x]",
        "in_progress" => "[>]",
        "pending" => "[ ]",
        "cancelled" => "[~]",
        _ => "[?]",
    }
}

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "pending" | "in_progress")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TodoStore {
    items: Arc<RwLock<Vec<TodoItem>>>,
    revision: Arc<RwLock<usize>>,
}

impl TodoStore {
    pub fn new() -> Self {
        Self {
            items: Arc::new(RwLock::new(Vec::new())),
            revision: Arc::new(RwLock::new(0)),
        }
    }

    pub async fn read(&self) -> Vec<TodoItem> {
        let lock = self.items.read().await;
        lock.clone()
    }

    pub async fn has_items(&self) -> bool {
        let lock = self.items.read().await;
        !lock.is_empty()
    }

    pub async fn revision(&self) -> usize {
        let rev = self.revision.read().await;
        *rev
    }

    pub async fn snapshot(&self) -> serde_json::Value {
        let items = self.read().await;
        let rev = self.revision().await;
        serde_json::json!({
            "todos": items,
            "revision": rev,
        })
    }

    pub async fn restore(
        &self,
        todos: &[serde_json::Value],
        revision: Option<usize>,
    ) -> Vec<TodoItem> {
        let mut fresh = Self::fresh_items(todos);
        if fresh.len() > MAX_TODO_ITEMS {
            fresh.truncate(MAX_TODO_ITEMS);
        }
        let mut items_lock = self.items.write().await;
        let mut rev_lock = self.revision.write().await;
        *items_lock = fresh;
        *rev_lock = revision.unwrap_or(0);
        items_lock.clone()
    }

    pub async fn write(&self, todos: &[serde_json::Value], merge: bool) -> Vec<TodoItem> {
        let mut items_lock = self.items.write().await;
        let before = items_lock.clone();

        if merge {
            Self::merge_into(&mut items_lock, todos);
        } else {
            *items_lock = Self::fresh_items(todos);
        }

        if items_lock.len() > MAX_TODO_ITEMS {
            items_lock.truncate(MAX_TODO_ITEMS);
        }
        Self::sanitize_parents(&mut items_lock);

        if *items_lock != before {
            let mut rev_lock = self.revision.write().await;
            *rev_lock += 1;
        }

        items_lock.clone()
    }

    pub async fn format_for_injection(&self) -> Option<String> {
        let items = self.read().await;
        if items.is_empty() {
            return None;
        }

        let mut children: HashMap<String, Vec<TodoItem>> = HashMap::new();
        for item in &items {
            if let Some(ref parent) = item.parent {
                children.entry(parent.clone()).or_default().push(item.clone());
            }
        }

        fn render(
            item: &TodoItem,
            depth: usize,
            children: &HashMap<String, Vec<TodoItem>>,
            out: &mut Vec<String>,
        ) -> bool {
            let mut kid_lines = Vec::new();
            let mut has_active_kid = false;

            if let Some(kids) = children.get(&item.id) {
                for kid in kids {
                    let kid_active = render(kid, depth + 1, children, &mut kid_lines);
                    has_active_kid = has_active_kid || kid_active;
                }
            }

            let keep = is_active_status(&item.status) || has_active_kid;
            if keep {
                let marker = status_marker(&item.status);
                let indent = "  ".repeat(depth);
                out.push(format!(
                    "{}- {} {}. {} ({})",
                    indent, marker, item.id, item.content, item.status
                ));
                out.extend(kid_lines);
            }
            keep
        }

        let mut lines = vec![TODO_INJECTION_HEADER.to_string()];
        for item in &items {
            if item.parent.is_none() {
                render(item, 0, &children, &mut lines);
            }
        }

        if lines.len() > 1 {
            Some(lines.join("\n"))
        } else {
            None
        }
    }

    pub fn cap_content(content: &str) -> String {
        if content.chars().count() > MAX_TODO_CONTENT_CHARS {
            let take_len = MAX_TODO_CONTENT_CHARS.saturating_sub(TRUNCATION_MARKER.chars().count());
            let head: String = content.chars().take(take_len).collect();
            format!("{}{}", head, TRUNCATION_MARKER)
        } else {
            content.to_string()
        }
    }

    pub fn validate(val: &serde_json::Value) -> TodoItem {
        let obj = match val.as_object() {
            Some(o) => o,
            None => {
                return TodoItem {
                    id: "?".to_string(),
                    content: "(invalid item)".to_string(),
                    status: "pending".to_string(),
                    parent: None,
                };
            }
        };

        let id = obj
            .get("id")
            .and_then(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else if v.is_number() || v.is_boolean() {
                    Some(v.to_string())
                } else {
                    None
                }
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "?".to_string());

        let raw_content = obj
            .get("content")
            .and_then(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else if v.is_number() || v.is_boolean() {
                    Some(v.to_string())
                } else {
                    None
                }
            })
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let content = if raw_content.is_empty() {
            "(no description)".to_string()
        } else {
            Self::cap_content(&raw_content)
        };

        let raw_status = obj
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_else(|| "pending".to_string());

        let status = if VALID_STATUSES.contains(&raw_status.as_str()) {
            raw_status
        } else {
            "pending".to_string()
        };

        let parent = obj
            .get("parent")
            .and_then(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else if v.is_number() {
                    Some(v.to_string())
                } else {
                    None
                }
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != &id);

        TodoItem {
            id,
            content,
            status,
            parent,
        }
    }

    fn dedupe_by_id(todos: &[serde_json::Value]) -> Vec<serde_json::Value> {
        let mut last_index: HashMap<String, usize> = HashMap::new();
        for (i, item) in todos.iter().enumerate() {
            let key = if let Some(obj) = item.as_object() {
                obj.get("id")
                    .and_then(|v| {
                        if let Some(s) = v.as_str() {
                            Some(s.to_string())
                        } else if v.is_number() || v.is_boolean() {
                            Some(v.to_string())
                        } else {
                            None
                        }
                    })
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "?".to_string())
            } else {
                format!("__invalid_{}", i)
            };
            last_index.insert(key, i);
        }

        let mut sorted_indices: Vec<usize> = last_index.into_values().collect();
        sorted_indices.sort_unstable();

        sorted_indices.into_iter().map(|idx| todos[idx].clone()).collect()
    }

    fn fresh_items(todos: &[serde_json::Value]) -> Vec<TodoItem> {
        let deduped = Self::dedupe_by_id(todos);
        let validated: Vec<TodoItem> = deduped.iter().map(Self::validate).collect();
        Self::normalize_order(validated)
    }

    fn merge_into(existing_items: &mut Vec<TodoItem>, todos: &[serde_json::Value]) {
        let mut existing_map: HashMap<String, TodoItem> = existing_items
            .iter()
            .map(|item| (item.id.clone(), item.clone()))
            .collect();

        let deduped = Self::dedupe_by_id(todos);
        for t in deduped {
            let (item_id, obj_opt) = match t.as_object() {
                Some(obj) => {
                    let id = obj
                        .get("id")
                        .and_then(|v| {
                            if let Some(s) = v.as_str() {
                                Some(s.to_string())
                            } else if v.is_number() || v.is_boolean() {
                                Some(v.to_string())
                            } else {
                                None
                            }
                        })
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default();
                    (id, Some(obj))
                }
                None => (String::new(), None),
            };

            if item_id.is_empty() {
                continue;
            }

            if let Some(cur) = existing_map.get_mut(&item_id) {
                if let Some(obj) = obj_opt {
                    if let Some(content_val) = obj.get("content") {
                        let c_str = match content_val.as_str() {
                            Some(s) => s.to_string(),
                            None => content_val.to_string(),
                        };
                        let trimmed = c_str.trim();
                        if !trimmed.is_empty() {
                            cur.content = Self::cap_content(trimmed);
                        }
                    }
                    if let Some(status_val) = obj.get("status").and_then(|v| v.as_str()) {
                        let lower = status_val.trim().to_lowercase();
                        if VALID_STATUSES.contains(&lower.as_str()) {
                            cur.status = lower;
                        }
                    }
                    if let Some(parent_val) = obj.get("parent") {
                        if parent_val.is_null() {
                            cur.parent = None;
                        } else {
                            let p_str = match parent_val.as_str() {
                                Some(s) => s.to_string(),
                                None => parent_val.to_string(),
                            };
                            let trimmed = p_str.trim();
                            if !trimmed.is_empty() && trimmed != item_id {
                                cur.parent = Some(trimmed.to_string());
                            } else {
                                cur.parent = None;
                            }
                        }
                    }
                }
            } else {
                let validated = Self::validate(&t);
                existing_map.insert(validated.id.clone(), validated.clone());
                existing_items.push(validated);
            }
        }

        // Rebuild preserving original order for existing items (first occurrence wins)
        let rebuilt: Vec<TodoItem> = existing_items
            .iter()
            .filter_map(|item| existing_map.get(&item.id).cloned())
            .collect();

        *existing_items = Self::normalize_order(rebuilt);
    }

    fn sanitize_parents(items: &mut [TodoItem]) {
        let existing_ids: HashSet<String> = items.iter().map(|item| item.id.clone()).collect();
        for item in items.iter_mut() {
            if let Some(ref p) = item.parent {
                if !existing_ids.contains(p) {
                    item.parent = None;
                }
            }
        }

        let parent_map: HashMap<String, Option<String>> = items
            .iter()
            .map(|item| (item.id.clone(), item.parent.clone()))
            .collect();

        for item in items.iter_mut() {
            let mut seen = HashSet::new();
            seen.insert(item.id.clone());
            let mut curr = item.parent.clone();

            while let Some(ref p) = curr {
                if seen.contains(p) {
                    item.parent = None;
                    break;
                }
                seen.insert(p.clone());
                curr = parent_map.get(p).cloned().flatten();
            }
        }
    }

    fn normalize_order(mut items: Vec<TodoItem>) -> Vec<TodoItem> {
        let has_any_parent = items.iter().any(|item| item.parent.is_some());
        let has_in_progress = items.iter().any(|item| item.status == "in_progress");

        if has_any_parent || !has_in_progress {
            return items;
        }

        let active_index = match items.iter().position(|item| item.status == "in_progress") {
            Some(idx) => idx,
            None => return items,
        };

        let first_pending = items.iter().position(|item| item.status == "pending");
        if let Some(pending_idx) = first_pending {
            if pending_idx < active_index {
                let item = items.remove(active_index);
                items.insert(pending_idx, item);
            }
        }

        items
    }
}

pub async fn todo_tool(
    todos_arg: Option<&serde_json::Value>,
    merge: bool,
    store: &TodoStore,
) -> Result<serde_json::Value, String> {
    let items = match todos_arg {
        None => store.read().await,
        Some(serde_json::Value::Null) => store.read().await,
        Some(val) => {
            let parsed_vec: Vec<serde_json::Value> = if let Some(arr) = val.as_array() {
                arr.clone()
            } else if let Some(s) = val.as_str() {
                match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(serde_json::Value::Array(arr)) => arr,
                    _ => {
                        return Err(
                            "todos must be a list of objects, got unparseable string".to_string(),
                        );
                    }
                }
            } else {
                return Err(format!(
                    "todos must be a list, got {}",
                    match val {
                        serde_json::Value::Object(_) => "dict",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::Bool(_) => "bool",
                        _ => "invalid",
                    }
                ));
            };

            store.write(&parsed_vec, merge).await
        }
    };

    let total = items.len();
    let pending = items.iter().filter(|i| i.status == "pending").count();
    let in_progress = items.iter().filter(|i| i.status == "in_progress").count();
    let completed = items.iter().filter(|i| i.status == "completed").count();
    let cancelled = items.iter().filter(|i| i.status == "cancelled").count();
    let rev = store.revision().await;

    Ok(serde_json::json!({
        "todos": items,
        "revision": rev,
        "summary": {
            "total": total,
            "pending": pending,
            "in_progress": in_progress,
            "completed": completed,
            "cancelled": cancelled
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_todo_crud_and_merge() {
        let store = TodoStore::new();

        let initial = vec![
            serde_json::json!({ "id": "1", "content": "Step 1", "status": "pending" }),
            serde_json::json!({ "id": "2", "content": "Step 2", "status": "pending" }),
        ];
        let items = store.write(&initial, false).await;
        assert_eq!(items.len(), 2);
        assert_eq!(store.revision().await, 1);

        let updates = vec![
            serde_json::json!({ "id": "1", "status": "in_progress" }),
            serde_json::json!({ "id": "3", "content": "Step 3", "status": "pending" }),
        ];
        let merged = store.write(&updates, true).await;
        assert_eq!(merged.len(), 3);
        assert_eq!(store.revision().await, 2);

        let item1 = merged.iter().find(|i| i.id == "1").unwrap();
        assert_eq!(item1.status, "in_progress");
        assert_eq!(item1.content, "Step 1");

        let item3 = merged.iter().find(|i| i.id == "3").unwrap();
        assert_eq!(item3.status, "pending");
        assert_eq!(item3.content, "Step 3");
    }

    #[tokio::test]
    async fn test_deduplication_and_order_normalization() {
        let store = TodoStore::new();

        let input = vec![
            serde_json::json!({ "id": "task-a", "content": "Old task A", "status": "pending" }),
            serde_json::json!({ "id": "task-b", "content": "Task B", "status": "pending" }),
            serde_json::json!({ "id": "task-a", "content": "Updated task A", "status": "completed" }),
            serde_json::json!({ "id": "task-c", "content": "Task C", "status": "in_progress" }),
        ];

        let res = store.write(&input, false).await;
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].id, "task-c");
        assert_eq!(res[0].status, "in_progress");

        assert_eq!(res[1].id, "task-b");
        assert_eq!(res[1].status, "pending");

        assert_eq!(res[2].id, "task-a");
        assert_eq!(res[2].content, "Updated task A");
        assert_eq!(res[2].status, "completed");
    }

    #[tokio::test]
    async fn test_parent_cycle_breaking() {
        let store = TodoStore::new();

        let cyclic = vec![
            serde_json::json!({ "id": "node-1", "content": "Node 1", "status": "pending", "parent": "node-3" }),
            serde_json::json!({ "id": "node-2", "content": "Node 2", "status": "pending", "parent": "node-1" }),
            serde_json::json!({ "id": "node-3", "content": "Node 3", "status": "pending", "parent": "node-2" }),
            serde_json::json!({ "id": "node-4", "content": "Node 4", "status": "pending", "parent": "missing-parent" }),
        ];

        let res = store.write(&cyclic, false).await;
        assert_eq!(res.len(), 4);

        let n4 = res.iter().find(|i| i.id == "node-4").unwrap();
        assert!(n4.parent.is_none());

        let has_broken_cycle = res.iter().any(|i| i.parent.is_none());
        assert!(has_broken_cycle);
    }

    #[tokio::test]
    async fn test_format_for_injection_hierarchy() {
        let store = TodoStore::new();

        let tasks = vec![
            serde_json::json!({ "id": "phase-1", "content": "Phase 1 Setup", "status": "completed" }),
            serde_json::json!({ "id": "task-1.1", "content": "Subtask 1", "status": "completed", "parent": "phase-1" }),
            serde_json::json!({ "id": "task-1.2", "content": "Subtask 2 Active", "status": "in_progress", "parent": "phase-1" }),
            serde_json::json!({ "id": "phase-2", "content": "Phase 2 Next", "status": "pending" }),
            serde_json::json!({ "id": "phase-3", "content": "Phase 3 Done", "status": "completed" }),
        ];

        store.write(&tasks, false).await;
        let formatted = store.format_for_injection().await;
        assert!(formatted.is_some());
        let text = formatted.unwrap();

        assert!(text.contains(TODO_INJECTION_HEADER));
        assert!(text.contains("- [x] phase-1. Phase 1 Setup (completed)"));
        assert!(text.contains("  - [>] task-1.2. Subtask 2 Active (in_progress)"));
        assert!(!text.contains("task-1.1"));
        assert!(text.contains("- [ ] phase-2. Phase 2 Next (pending)"));
        assert!(!text.contains("phase-3"));
    }
}
