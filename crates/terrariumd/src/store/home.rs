//! Home projections. Each page examines at most 100 indexed candidates.
//! The continuation cursor advances over candidates, including filtered rows.
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Page, board, projects};
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeBoardColumns {
    pub in_progress: Vec<String>,
    pub review: Vec<String>,
    pub accepted: Vec<String>,
    pub blocked: Vec<String>,
}

impl Default for HomeBoardColumns {
    fn default() -> Self {
        Self {
            in_progress: vec!["working".into(), "research".into()],
            review: vec!["review".into(), "merge_queue".into()],
            accepted: vec!["accepted".into()],
            blocked: vec!["blocked".into()],
        }
    }
}

impl HomeBoardColumns {
    pub fn validate(&self) -> ApiResult<()> {
        for values in [&self.in_progress, &self.review, &self.accepted, &self.blocked] {
            if values.len() > 32 || values.iter().any(|s| s.trim().is_empty() || s.len() > 128) {
                return Err(ApiError::invalid("Home columns need at most 32 nonempty role or name selectors, each at most 128 bytes"));
            }
        }
        Ok(())
    }

    fn selectors(&self, column: &str) -> ApiResult<&[String]> {
        match column {
            "in_progress" => Ok(&self.in_progress),
            "review" => Ok(&self.review),
            "accepted" => Ok(&self.accepted),
            "blocked" => Ok(&self.blocked),
            _ => Err(ApiError::invalid("Home column must be in_progress, review, accepted or blocked")),
        }
    }

    fn matches(&self, column: &str, name: &str, columns: &[(String, Option<String>)]) -> ApiResult<bool> {
        Ok(self.selectors(column)?.iter().any(|selector| {
            // A role wins over a same-named column. Names are case insensitive.
            let by_role = columns.iter().find(|(_, role)| role.as_deref() == Some(selector.as_str()));
            by_role.or_else(|| columns.iter().find(|(n, _)| n.eq_ignore_ascii_case(selector)))
                .is_some_and(|(n, _)| n == name)
        }))
    }
}

#[derive(Debug, Serialize)]
pub struct HomePage {
    pub items: Vec<Value>,
    pub next: Option<String>,
}

// Separate project and global forms keep both seeks indexed. Never interpolate
// caller text into SQL. Only the three literal table names below reach this.
fn candidates_sql(table: &str, project: bool) -> String {
    let active = if table == "cards" { "archived = 0 AND " } else { "" };
    let scope = if project { "project = ?3 AND " } else { "" };
    format!("SELECT id FROM {table} WHERE {scope}{active}id > ?1 ORDER BY id LIMIT ?2")
}

const CARD_SQL: &str = "SELECT c.id, c.title, c.labels, c.project, c.assignee,
 a.name, a.status, c.eta_at, k.name FROM cards c
 JOIN board_columns k ON k.id = c.column_id LEFT JOIN agents a ON a.id = c.assignee
 WHERE c.id = ?1";
const MONITOR_SQL: &str = "SELECT m.id, m.project, m.name,
 COALESCE((SELECT status FROM monitor_results WHERE monitor = m.id ORDER BY pk DESC LIMIT 1), 'unknown')
 FROM monitors m WHERE m.id = ?1";
const PIPELINE_SQL: &str = "SELECT p.id, p.project, p.name,
 (SELECT status FROM pipeline_runs WHERE pipeline = p.id ORDER BY sort_at DESC, ord ASC, pk DESC LIMIT 1)
 FROM pipelines p WHERE p.id = ?1";
const COMMENT_SQL: &str = "SELECT body FROM card_comments WHERE card = ?1 ORDER BY created_at DESC LIMIT 1";

/// `project` has already been restricted to the caller's readable scope.
pub fn list(conn: &Connection, kind: &str, column: &str, project: Option<&str>, page: &Page) -> ApiResult<HomePage> {
    let table = match kind {
        "cards" => { HomeBoardColumns::default().selectors(column)?; "cards" },
        "monitors" => "monitors",
        "pipelines" => "pipelines",
        _ => return Err(ApiError::not_found("no such Home list")),
    };
    let limit = page.limit().min(100);
    let sql = candidates_sql(table, project.is_some());
    let mut stmt = conn.prepare(&sql)?;
    let cursor = page.cursor().unwrap_or("");
    let count = limit as i64 + 1;
    let args: Vec<&dyn rusqlite::ToSql> = if let Some(ref project) = project {
        vec![&cursor, &count, project]
    } else {
        vec![&cursor, &count]
    };
    let mut ids = stmt.query_map(args.as_slice(), |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let more = ids.len() > limit;
    ids.truncate(limit);
    let next = if more { ids.last().cloned() } else { None };
    let mut items = Vec::new();
    for id in ids {
        if kind == "cards" {
            let mut item = conn.query_row(CARD_SQL, [&id], |r| Ok(json!({
                "id": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?,
                "labels": super::json_col::<Vec<String>>(&r.get::<_, String>(2)?),
                "project": r.get::<_, String>(3)?, "assignee": r.get::<_, Option<String>>(4)?,
                "assignee_name": r.get::<_, Option<String>>(5)?, "assignee_status": r.get::<_, Option<String>>(6)?,
                "eta_at": r.get::<_, Option<String>>(7)?, "column": r.get::<_, String>(8)?,
                "home_column": column
            })))?;
            let project = item["project"].as_str().unwrap_or_default();
            let settings = projects::get(conn, project)?.home_board_columns;
            if !settings.matches(column, item["column"].as_str().unwrap_or_default(), &board::column_roles(conn, project)?)? {
                continue;
            }
            if column == "blocked" {
                let moved: Option<Option<String>> = conn.query_row(board::BLOCKED_INFO_SQL, params![id, board::BLOCKED], |r| r.get(1)).optional()?;
                let reason = moved.flatten().filter(|s| !s.trim().is_empty()).or(conn.query_row(COMMENT_SQL, [&id], |r| r.get::<_, String>(0)).optional()?);
                item["blocked_reason"] = reason.and_then(|s| s.lines().next().map(str::to_owned)).into();
            }
            items.push(item);
        } else {
            let sql = if kind == "monitors" { MONITOR_SQL } else { PIPELINE_SQL };
            let (project, name, status): (String, String, Option<String>) = conn.query_row(sql, [&id], |r| Ok((r.get(1)?, r.get(2)?, r.get(3)?)))?;
            let attention = if kind == "monitors" { matches!(status.as_deref(), Some("warn" | "crit" | "unknown")) } else { status.as_deref() == Some("failed") };
            if attention {
                items.push(json!({"id": id, "project": project, "name": name, "status": status,
                    "link": format!("/p/{project}/{}", if kind == "monitors" { "dashboard" } else { "pipelines" })}));
            }
        }
    }
    Ok(HomePage { items, next })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn defaults_missing_columns_and_role_precedence() {
        let settings = HomeBoardColumns::default();
        let columns = vec![("In Progress".into(), Some("working".into())), ("review".into(), None), ("accepted".into(), None)];
        assert!(settings.matches("in_progress", "In Progress", &columns).unwrap());
        assert!(settings.matches("review", "review", &columns).unwrap());
        assert!(settings.matches("accepted", "accepted", &columns).unwrap());
        assert!(!settings.matches("blocked", "review", &columns).unwrap());
        let columns = vec![("working".into(), None), ("custom".into(), Some("working".into()))];
        assert!(!settings.matches("in_progress", "working", &columns).unwrap());
        assert!(settings.matches("in_progress", "custom", &columns).unwrap());
        assert!(settings.selectors("bad").is_err());
    }

    #[tokio::test]
    async fn settings_persist_and_project_filter_and_candidate_pagination() {
        let db = crate::db::Db::new(conn(), crate::events::EventHub::default());
        db.write("operator", |t| {
            for name in ["alpha", "beta"] {
                projects::create(t, &projects::NewProject { name: name.into(), ..Default::default() }, projects::SOURCE_API)?;
                for column in ["ready", "in_progress", "blocked"] {
                    board::create_card(t, &crate::actor::Actor::Operator, name, board::NewCard {
                        title: Some(format!("{name} {column}")), column: Some(column.into()),
                        labels: vec!["type:impl".into()], comment: Some("Need a lease\nMore detail".into()), ..Default::default()
                    })?;
                }
            }
            let patch: projects::ProjectPatch = serde_json::from_value(json!({"home_board_columns": {
                "in_progress": ["ready"], "review": [], "accepted": [], "blocked": ["blocked"]
            }})).unwrap();
            let p = projects::update(t, "alpha", &patch)?;
            assert_eq!(p.home_board_columns.in_progress, vec!["ready"]);
            Ok(())
        }).await.unwrap();
        db.read(|c| {
            let all = list(c, "cards", "in_progress", None, &Page::default())?;
            assert_eq!(all.items.len(), 2);
            assert_eq!(all.items[0]["labels"], json!(["type:impl"]));
            let alpha = list(c, "cards", "in_progress", Some("alpha"), &Page::default())?;
            assert_eq!(alpha.items.len(), 1);
            assert_eq!(alpha.items[0]["title"], "alpha ready");
            let blocked = list(c, "cards", "blocked", None, &Page::default())?;
            assert_eq!(blocked.items.len(), 2);
            assert_eq!(blocked.items[0]["blocked_reason"], "Need a lease");
            let mut cursor = None;
            let mut count = 0;
            let mut pages = 0;
            loop {
                let page = list(c, "cards", "in_progress", None, &Page { limit: Some(1), cursor })?;
                count += page.items.len();
                pages += 1;
                cursor = page.next;
                if cursor.is_none() { break; }
                assert!(pages < 10);
            }
            assert_eq!(count, 2);
            assert_eq!(pages, 6);
            Ok(())
        }).await.unwrap();
    }

    #[test]
    fn attention_uses_latest_results_without_dashboard_membership() {
        let c = conn();
        for project in ["alpha", "beta"] {
            c.execute("INSERT INTO projects(name, created_at, updated_at) VALUES (?1, 'x', 'x')", [project]).unwrap();
        }
        for (id, project) in [("a", "alpha"), ("b", "beta"), ("c", "alpha"), ("d", "alpha"), ("e", "alpha")] {
            c.execute("INSERT INTO monitors(id,project,name,kind,def,created_by,run_as,created_at,updated_at,anchor_at) VALUES (?1,?2,?1,'status','{}','operator','operator','x','x','x')", params![id, project]).unwrap();
        }
        for (id, status) in [("a", "crit"), ("b", "warn"), ("c", "crit"), ("c", "ok"), ("e", "unknown")] {
            c.execute("INSERT INTO monitor_results(monitor,at,status,duration_ms) VALUES (?1,'x',?2,1)", params![id, status]).unwrap();
        }
        assert_eq!(list(&c, "monitors", "", None, &Page::default()).unwrap().items.len(), 4);
        assert_eq!(list(&c, "monitors", "", Some("beta"), &Page::default()).unwrap().items.len(), 1);
        for (id, project) in [("a", "alpha"), ("b", "beta"), ("c", "alpha")] {
            c.execute("INSERT INTO pipelines(id,project,name,adapter,def,created_by,run_as,created_at,updated_at,anchor_at) VALUES (?1,?2,?1,'script','{}','operator','operator','x','x','x')", params![id, project]).unwrap();
        }
        for (id, run, status, sort) in [("a", "1", "failed", "1"), ("b", "1", "failed", "1"), ("b", "2", "success", "2")] {
            c.execute("INSERT INTO pipeline_runs(pipeline,run_id,status,sort_at,first_seen,changed_at,seen_at) VALUES (?1,?2,?3,?4,'x','x','x')", params![id, run, status, sort]).unwrap();
        }
        let failed = list(&c, "pipelines", "", None, &Page::default()).unwrap();
        assert_eq!(failed.items.len(), 1);
        assert_eq!(failed.items[0]["id"], "a");
        assert_eq!(failed.items[0]["link"], "/p/alpha/pipelines");
        assert!(list(&c, "pipelines", "", Some("beta"), &Page::default()).unwrap().items.is_empty());
    }

    #[test]
    fn every_home_query_is_an_indexed_seek_without_a_sort() {
        let c = conn();
        for table in ["cards", "monitors", "pipelines"] {
            for project in [false, true] {
                let sql = format!("EXPLAIN QUERY PLAN {}", candidates_sql(table, project));
                let mut stmt = c.prepare(&sql).unwrap();
                let args: Vec<&dyn rusqlite::ToSql> = if project { vec![&"", &101, &"alpha"] } else { vec![&"", &101] };
                let plan = stmt.query_map(args.as_slice(), |r| r.get::<_, String>(3)).unwrap().collect::<Result<Vec<_>, _>>().unwrap().join("\n");
                assert!(plan.contains("SEARCH"), "{plan}");
                assert!(!plan.contains("SCAN ") && !plan.contains("TEMP B-TREE"), "{plan}");
            }
        }
        for sql in [CARD_SQL, MONITOR_SQL, PIPELINE_SQL, COMMENT_SQL] {
            let plan = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap().query_map(["id"], |r| r.get::<_, String>(3)).unwrap().collect::<Result<Vec<_>, _>>().unwrap().join("\n");
            assert!(plan.contains("SEARCH"), "{plan}");
            assert!(!plan.contains("SCAN ") && !plan.contains("TEMP B-TREE"), "{plan}");
        }
    }
}
