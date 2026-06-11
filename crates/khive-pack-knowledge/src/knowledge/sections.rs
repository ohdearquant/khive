//! Section handlers: edit, import, challenge, adjudicate; markdown parsing helpers.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::schema::{
    AdjudicateParams, ChallengeParams, EditParams, ImportParams, Section, SectionType,
};
use super::util::resolve_atom_id;
use super::util::{
    content_hash, deser, new_id, now_us, row_str, sql_err, validate_section_content,
};
use super::KnowledgeHandlers;

// ─── section helpers ──────────────────────────────────────────────────────────

fn count_tokens(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

fn parse_section_type(s: &str) -> Result<SectionType, RuntimeError> {
    SectionType::from_str_loose(s).ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "unknown section_type {s:?}; valid values: {}",
            SectionType::NAMES.join(", ")
        ))
    })
}

// REASON: section_from_row and section_to_json are forward-deployed helpers for the
// section-read verb surface (Phase 3); retained so the implementation
// compiles without gaps when that verb lands.
#[allow(dead_code)]
fn section_from_row(row: &khive_storage::types::SqlRow) -> Option<Section> {
    let id: Uuid = row_str(row, "id")?.parse().ok()?;
    let st_str = row_str(row, "section_type")?;
    let section_type = SectionType::from_str_loose(&st_str)?;
    Some(Section {
        id,
        atom_id: row_str(row, "atom_id")?,
        namespace: row_str(row, "namespace")?,
        section_type,
        heading: row_str(row, "heading").unwrap_or_default(),
        content: row_str(row, "content").unwrap_or_default(),
        content_hash: row_str(row, "content_hash").unwrap_or_default(),
        status: row_str(row, "status").unwrap_or_else(|| "draft".into()),
        tokens: super::util::row_i64(row, "tokens").unwrap_or(0),
        sort_order: super::util::row_i64(row, "sort_order").unwrap_or(0),
        created_at: super::util::row_i64(row, "created_at").unwrap_or(0),
        updated_at: super::util::row_i64(row, "updated_at").unwrap_or(0),
    })
}

// REASON: forward-deployed helper; see comment on section_from_row above.
#[allow(dead_code)]
fn section_to_json(s: &Section) -> Value {
    json!({
        "id": s.id.to_string(),
        "atom_id": s.atom_id,
        "namespace": s.namespace,
        "section_type": s.section_type.as_str(),
        "heading": s.heading,
        "content": s.content,
        "tokens": s.tokens,
        "sort_order": s.sort_order,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

// ─── markdown parsing helpers ─────────────────────────────────────────────────

fn collect_md_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_md_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
}

fn to_slug(stem: &str) -> String {
    stem.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn extract_atlas_id(content: &str) -> Option<String> {
    content.lines().take(32).find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("atlas_id:")
            .or_else(|| trimmed.strip_prefix("atlas-id:"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn parse_atlas_md(content: &str) -> (String, String, Vec<(SectionType, String, String)>) {
    let mut name = String::new();
    let mut pre_body = String::new();
    let mut sections: Vec<(SectionType, String, String)> = Vec::new();

    let mut current_heading: Option<(SectionType, String)> = None;
    let mut current_body = String::new();
    let mut in_pre = true;

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if name.is_empty() && current_heading.is_none() && current_body.trim().is_empty() {
                name = rest.trim().to_string();
                continue;
            }
        }

        if let Some(rest) = line.strip_prefix("## ") {
            if let Some((stype, heading)) = current_heading.take() {
                sections.push((stype, heading, current_body.trim_end().to_string()));
                current_body.clear();
            } else if in_pre {
                pre_body = current_body.trim_end().to_string();
                current_body.clear();
                in_pre = false;
            }
            let heading_text = rest.trim().to_string();
            let stype = SectionType::from_str_loose(&heading_text).unwrap_or(SectionType::Other);
            current_heading = Some((stype, heading_text));
            continue;
        }
        current_body.push_str(line);
        current_body.push('\n');
    }

    if let Some((stype, heading)) = current_heading {
        sections.push((stype, heading, current_body.trim_end().to_string()));
    } else {
        pre_body = current_body.trim_end().to_string();
    }

    (name, pre_body, sections)
}

// ─── handler impls ────────────────────────────────────────────────────────────

impl KnowledgeHandlers {
    pub(crate) async fn edit(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: EditParams = deser(params)?;
        if p.sections.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "sections must not be empty".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let atom_id = {
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("edit atom reader", e))?;
            let id = p.id.trim().to_string();
            let row = if id.parse::<Uuid>().is_ok() {
                reader
                    .query_row(SqlStatement {
                        sql: "SELECT id FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit atom lookup by id", e))?
            } else {
                reader
                    .query_row(SqlStatement {
                        sql: "SELECT id FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit atom lookup by slug", e))?
            };
            row.and_then(|r| row_str(&r, "id"))
                .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {:?}", p.id)))?
        };

        let now = now_us();
        let mut upserted = 0usize;
        let mut section_results: Vec<Value> = Vec::with_capacity(p.sections.len());

        for su in &p.sections {
            let stype = parse_section_type(&su.section_type)?;
            validate_section_content(&su.content)?;
            // Secret gate: scan section content and heading before any write.
            khive_runtime::secret_gate::check(&su.content)?;
            if let Some(ref h) = su.heading {
                khive_runtime::secret_gate::check(h)?;
            }
            let heading = su.heading.as_deref().unwrap_or(stype.as_str()).to_string();
            let tokens = count_tokens(&su.content);
            let sort_order = su.sort_order.unwrap_or_else(|| {
                SectionType::ALL
                    .iter()
                    .position(|&t| t == stype)
                    .unwrap_or(9) as i64
            });
            let hash = content_hash(&su.content);

            // Sections are content-addressed: the dedup key is (atom_id, content_hash),
            // matching the UNIQUE constraint. Identical content is an idempotent
            // metadata refresh; distinct content inserts a new row, so repeated
            // section types with differing content coexist as sibling rows.
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("edit section reader", e))?;
            let existing_section = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_sections \
                          WHERE atom_id = ?1 AND content_hash = ?2 LIMIT 1"
                        .into(),
                    params: vec![
                        SqlValue::Text(atom_id.clone()),
                        SqlValue::Text(hash.clone()),
                    ],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("edit section lookup", e))?;

            let section_id = existing_section
                .as_ref()
                .and_then(|r| row_str(r, "id"))
                .unwrap_or_else(new_id);

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("edit section writer", e))?;

            if existing_section.is_some() {
                // Identical content already stored: refresh metadata only. Content
                // is unchanged, so the embedding and verification status stay valid.
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE knowledge_sections SET \
                              heading=?1, tokens=?2, sort_order=?3, updated_at=?4 \
                              WHERE id=?5"
                            .into(),
                        params: vec![
                            SqlValue::Text(heading.clone()),
                            SqlValue::Integer(tokens),
                            SqlValue::Integer(sort_order),
                            SqlValue::Integer(now),
                            SqlValue::Text(section_id.clone()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit section update", e))?;
            } else {
                // New content: insert a fresh row, leaving any sibling sections
                // (including verified ones of the same type) untouched.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_sections \
                              (id, atom_id, namespace, section_type, heading, content, \
                               content_hash, tokens, sort_order, created_at, updated_at) \
                              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
                            .into(),
                        params: vec![
                            SqlValue::Text(section_id.clone()),
                            SqlValue::Text(atom_id.clone()),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(stype.as_str().to_string()),
                            SqlValue::Text(heading.clone()),
                            SqlValue::Text(su.content.clone()),
                            SqlValue::Text(hash.clone()),
                            SqlValue::Integer(tokens),
                            SqlValue::Integer(sort_order),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit section insert", e))?;
            }

            upserted += 1;
            section_results.push(json!({
                "id": section_id,
                "atom_id": atom_id,
                "section_type": stype.as_str(),
                "heading": heading,
                "tokens": tokens,
                "content_hash": hash,
            }));
        }

        Ok(json!({
            "atom_id": atom_id,
            "upserted": upserted,
            "sections": section_results,
        }))
    }

    pub(crate) async fn import(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ImportParams = deser(params)?;
        let path_str = p.path.trim().to_string();
        if path_str.is_empty() {
            return Err(RuntimeError::InvalidInput("path must not be empty".into()));
        }

        let chunk_strategy = p
            .chunk_strategy
            .as_deref()
            .unwrap_or("section")
            .to_ascii_lowercase();
        if !["section", "atom"].contains(&chunk_strategy.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown chunk_strategy {:?}; valid: section | atom",
                chunk_strategy
            )));
        }
        let format = p.format.as_deref().unwrap_or("atlas_md");
        if format != "atlas_md" {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown format {format:?}; only \"atlas_md\" is supported"
            )));
        }

        let md_path = std::path::Path::new(&path_str);
        if !md_path.exists() {
            return Err(RuntimeError::NotFound(format!(
                "path does not exist: {path_str:?}"
            )));
        }

        let files: Vec<std::path::PathBuf> = if md_path.is_file() {
            vec![md_path.to_path_buf()]
        } else if md_path.is_dir() {
            let mut v = Vec::new();
            collect_md_files(md_path, &mut v);
            v
        } else {
            return Err(RuntimeError::InvalidInput(format!(
                "path is not a file or directory: {path_str:?}"
            )));
        };

        if files.is_empty() {
            return Ok(json!({
                "imported_atoms": 0,
                "imported_sections": 0,
                "files_processed": 0,
            }));
        }

        let mut imported_atoms = 0usize;
        let mut imported_sections = 0usize;

        for file in &files {
            let content = std::fs::read_to_string(file)
                .map_err(|e| RuntimeError::Internal(format!("failed to read {:?}: {e}", file)))?;

            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let slug = to_slug(stem);

            let (atom_name, atom_body, sections) = parse_atlas_md(&content);
            let name = if atom_name.is_empty() {
                slug.replace('-', " ")
            } else {
                atom_name
            };

            // Atom content is its description. A section-only document (e.g.
            // `# Title` followed entirely by `##` sections) has an empty/short
            // pre-section body, which would fail the atom content minimum before
            // the sections are imported. Synthesize atom content from the section
            // bodies in that case so the atom carries meaningful text.
            let atom_content =
                if atom_body.split_whitespace().count() >= super::util::MIN_ATOM_CONTENT_WORDS {
                    atom_body
                } else {
                    sections
                        .iter()
                        .map(|(_, _, body)| body.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                };

            let atlas_id = extract_atlas_id(&content);
            let citation_count = sections
                .iter()
                .filter(|(stype, _, _)| *stype == SectionType::References)
                .map(|(_, _, body)| body.lines().filter(|line| !line.trim().is_empty()).count())
                .sum::<usize>();
            let source_uri = atlas_id.as_ref().map(|id| format!("atlas:{id}"));
            let source_type = if citation_count > 0 {
                "paper"
            } else {
                "imported"
            };
            let mut properties = serde_json::Map::new();
            if let Some(ref id) = atlas_id {
                properties.insert("atlas_id".to_string(), Value::String(id.clone()));
            }

            let upsert_params = serde_json::json!({
                "atoms": [{
                    "slug": slug,
                    "name": name,
                    "content": atom_content,
                    "properties": Value::Object(properties),
                    "source_uri": source_uri,
                    "source_type": source_type,
                }]
            });
            KnowledgeHandlers::upsert_atoms(runtime, token, upsert_params).await?;
            imported_atoms += 1;

            if chunk_strategy == "section" && !sections.is_empty() {
                // Filter out stub sections below the 80-char minimum to avoid
                // errors during import of markdown files with short sections.
                let section_updates: Vec<Value> = sections
                    .iter()
                    .filter(|(_, _, body)| body.len() >= super::util::MIN_SECTION_CONTENT_LEN)
                    .map(|(stype, heading, body)| {
                        json!({
                            "section_type": stype.as_str(),
                            "heading": heading,
                            "content": body,
                        })
                    })
                    .collect();
                if !section_updates.is_empty() {
                    let edit_params = json!({
                        "id": slug,
                        "sections": section_updates,
                    });
                    let result = KnowledgeHandlers::edit(runtime, token, edit_params).await?;
                    if let Some(n) = result.get("upserted").and_then(|v| v.as_u64()) {
                        imported_sections += n as usize;
                    }
                }
            }
        }

        Ok(json!({
            "imported_atoms": imported_atoms,
            "imported_sections": imported_sections,
            "files_processed": files.len(),
        }))
    }

    pub(crate) async fn challenge(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ChallengeParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let atom_id = resolve_atom_id(runtime, &ns, &p.atom_id).await?;
        let stype = parse_section_type(&p.section_type)?;
        let hash = p
            .content_hash
            .as_ref()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());

        // Same-type sibling sections are valid (UNIQUE(atom_id, content_hash)),
        // so section_type alone no longer identifies one section. Resolve the
        // single eligible target before mutating: a content_hash pins it
        // exactly, otherwise there must be exactly one eligible section.
        let target_hash = Self::resolve_section_hash(
            runtime,
            &atom_id,
            stype,
            hash.as_deref(),
            "status NOT IN ('disputed','deprecated')",
            "section not found, already disputed, or deprecated",
        )
        .await?;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("challenge writer", e))?;

        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_sections SET status='disputed' \
                      WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 \
                      AND status NOT IN ('disputed','deprecated')"
                    .into(),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(stype.as_str().to_string()),
                    SqlValue::Text(target_hash.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("challenge section status", e))?;

        if affected == 0 {
            return Err(RuntimeError::InvalidInput(
                "section not found, already disputed, or deprecated".into(),
            ));
        }

        writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',coalesce(json_extract(properties,'$.dispute_count'),0)+{affected}) WHERE id=?1 AND namespace=?2"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("challenge dispute_count increment", e))?;

        Ok(json!({
            "atom_id": atom_id,
            "section_type": stype.as_str(),
            "content_hash": target_hash,
            "disputed": affected,
            "reason": p.reason,
        }))
    }

    /// Resolve the single section of `stype` on `atom_id` that the lifecycle
    /// verbs should act on. `hash` pins an exact sibling; without it there must
    /// be exactly one section matching `status_filter`, otherwise the call is
    /// ambiguous and is rejected. Returns the target `content_hash`.
    async fn resolve_section_hash(
        runtime: &KhiveRuntime,
        atom_id: &str,
        stype: SectionType,
        hash: Option<&str>,
        status_filter: &str,
        not_found_msg: &str,
    ) -> Result<String, RuntimeError> {
        let sql = runtime.sql();
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("section resolve reader", e))?;

        let mut query = format!(
            "SELECT content_hash FROM knowledge_sections \
             WHERE atom_id=?1 AND section_type=?2 AND {status_filter}"
        );
        let mut params = vec![
            SqlValue::Text(atom_id.to_owned()),
            SqlValue::Text(stype.as_str().to_string()),
        ];
        if let Some(h) = hash {
            query.push_str(" AND content_hash=?3");
            params.push(SqlValue::Text(h.to_owned()));
        }

        let rows = reader
            .query_all(SqlStatement {
                sql: query,
                params,
                label: None,
            })
            .await
            .map_err(|e| sql_err("section resolve", e))?;

        if rows.is_empty() {
            return Err(RuntimeError::InvalidInput(not_found_msg.to_owned()));
        }
        if hash.is_none() && rows.len() > 1 {
            return Err(RuntimeError::InvalidInput(format!(
                "atom has {} '{}' sections matching; specify content_hash to target one",
                rows.len(),
                stype.as_str(),
            )));
        }
        rows.first()
            .and_then(|r| row_str(r, "content_hash"))
            .ok_or_else(|| RuntimeError::Internal("section row missing content_hash".into()))
    }

    pub(crate) async fn adjudicate(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: AdjudicateParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let resolution = p.resolution.trim().to_ascii_lowercase();
        if resolution != "accept" && resolution != "reject" {
            return Err(RuntimeError::InvalidInput(
                "resolution must be \"accept\" or \"reject\"".into(),
            ));
        }

        let atom_id = resolve_atom_id(runtime, &ns, &p.atom_id).await?;
        let stype = parse_section_type(&p.section_type)?;
        let hash = p
            .content_hash
            .as_ref()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());

        let new_status = if resolution == "accept" {
            "verified"
        } else {
            "reviewed"
        };

        // Target a single disputed section — same-type siblings can be disputed
        // independently, so resolve one before resolving its lifecycle.
        let target_hash = Self::resolve_section_hash(
            runtime,
            &atom_id,
            stype,
            hash.as_deref(),
            "status='disputed'",
            "section not found or not in disputed state",
        )
        .await?;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("adjudicate writer", e))?;

        let affected = writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_sections SET status='{new_status}' \
                     WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 AND status='disputed'"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(stype.as_str().to_string()),
                    SqlValue::Text(target_hash.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("adjudicate section status", e))?;

        if affected == 0 {
            return Err(RuntimeError::InvalidInput(
                "section not found or not in disputed state".into(),
            ));
        }

        writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',CASE WHEN coalesce(json_extract(properties,'$.dispute_count'),0) >= {affected} THEN coalesce(json_extract(properties,'$.dispute_count'),0)-{affected} ELSE 0 END) WHERE id=?1 AND namespace=?2"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("adjudicate dispute_count decrement", e))?;

        Ok(json!({
            "atom_id": atom_id,
            "section_type": stype.as_str(),
            "content_hash": target_hash,
            "resolution": resolution,
            "new_status": new_status,
            "resolved": affected,
        }))
    }
}
