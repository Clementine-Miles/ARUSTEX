use std::collections::{BTreeMap, BTreeSet};

use arustex_core::domain::asset::{
    Asset, AssetEdge, AssetGraph, AssetGraphIssue, AssetKind, AssetRelation, NewAsset,
};
use arustex_core::domain::exploration::{
    Anchor, EdgeRelation, ExplorationEdge, ExplorationIssue, ExplorationNode, ExplorationSnapshot,
    NewNode, NodeKind,
};
use arustex_core::domain::{AssetId, ExplorationId, NodeId, Origin, Provenance, RecordId, TaskId};
use arustex_core::ipc::AssetDetail;
use arustex_core::{CoreError, CoreResult, Page, Paged};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, db_err, to_u64};

/// Upper bound on nodes loaded for an in-memory consistency check.
pub const MAX_GRAPH_CHECK_NODES: i64 = 200_000;

fn asset_from_row(row: &PgRow) -> CoreResult<Asset> {
    let kind: String = row.try_get("kind").map_err(db_err)?;
    let origin: String = row.try_get("origin").map_err(db_err)?;
    Ok(Asset {
        id: AssetId(row.try_get("id").map_err(db_err)?),
        kind: kind.parse()?,
        identity_key: row.try_get("identity_key").map_err(db_err)?,
        label: row.try_get("label").map_err(db_err)?,
        attributes: row.try_get("attributes").map_err(db_err)?,
        provenance: Provenance {
            origin: origin.parse()?,
            source: row.try_get("source").map_err(db_err)?,
            locator: row.try_get("locator").map_err(db_err)?,
            session_id: None,
        },
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
    })
}

fn node_from_row(row: &PgRow) -> CoreResult<ExplorationNode> {
    let kind: String = row.try_get("kind").map_err(db_err)?;
    Ok(ExplorationNode {
        id: NodeId(row.try_get("id").map_err(db_err)?),
        exploration_id: ExplorationId(row.try_get("exploration_id").map_err(db_err)?),
        kind: kind.parse()?,
        state: row.try_get("state").map_err(db_err)?,
        priority: row.try_get("priority").map_err(db_err)?,
        title: row.try_get("title").map_err(db_err)?,
        payload: row.try_get("payload").map_err(db_err)?,
        origin: row.try_get("origin").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
        completed_at: row.try_get("completed_at").map_err(db_err)?,
    })
}

/// Outcome of an asset upsert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted(AssetId),
    Merged(AssetId),
}

impl UpsertOutcome {
    pub fn id(self) -> AssetId {
        match self {
            Self::Inserted(id) | Self::Merged(id) => id,
        }
    }
}

impl Store {
    /// Transactional upsert deduplicated by `(kind, normalized identity key)`. Attributes
    /// of an existing asset are merged (new keys win); provenance of the first import is kept.
    pub async fn upsert_asset(&self, input: &NewAsset) -> CoreResult<UpsertOutcome> {
        input.validate()?;
        let key = input.normalized_key();
        let row = sqlx::query(
            "INSERT INTO assets (id, kind, identity_key, label, attributes, origin, source, locator, legacy_id)
             VALUES (COALESCE($1, nextval(pg_get_serial_sequence('assets', 'id'))), $2, $3, $4, $5, $6, $7, $8, $1)
             ON CONFLICT (kind, identity_key) DO UPDATE
                 SET attributes = assets.attributes || EXCLUDED.attributes,
                     label = EXCLUDED.label
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(input.preserve_id.map(|i| i.0))
        .bind(input.kind.as_str())
        .bind(&key)
        .bind(input.label.trim())
        .bind(&input.attributes)
        .bind(input.provenance.origin.as_str())
        .bind(&input.provenance.source)
        .bind(&input.provenance.locator)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let id = AssetId(row.try_get("id").map_err(db_err)?);
        let inserted: bool = row.try_get("inserted").map_err(db_err)?;
        Ok(if inserted {
            UpsertOutcome::Inserted(id)
        } else {
            UpsertOutcome::Merged(id)
        })
    }

    /// Adds an asset relationship, rejecting containment cycles and second parents.
    pub async fn add_asset_edge(&self, edge: AssetEdge) -> CoreResult<()> {
        if edge.src == edge.dst {
            return Err(CoreError::invalid("an asset cannot relate to itself"));
        }
        let mut tx = self.begin().await?;
        // Serialize containment changes so concurrent inserts cannot form a cycle.
        sqlx::query("SELECT pg_advisory_xact_lock(7405623342)")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        match edge.rel {
            AssetRelation::Contains => {
                let cycle: bool = sqlx::query_scalar(
                    "WITH RECURSIVE up(id) AS (
                         SELECT $1::bigint
                         UNION
                         SELECT e.src_id FROM asset_edges e JOIN up ON e.dst_id = up.id
                         WHERE e.rel = 'contains'
                     ) SELECT EXISTS (SELECT 1 FROM up WHERE id = $2)",
                )
                .bind(edge.src.0)
                .bind(edge.dst.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                if cycle {
                    return Err(CoreError::Integrity(format!(
                        "containment {} -> {} would create a cycle",
                        edge.src, edge.dst
                    )));
                }
                let other_parent: Option<i64> = sqlx::query_scalar(
                    "SELECT src_id FROM asset_edges WHERE dst_id = $1 AND rel = 'contains' AND src_id <> $2",
                )
                .bind(edge.dst.0)
                .bind(edge.src.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                if let Some(parent) = other_parent {
                    return Err(CoreError::Conflict(format!(
                        "asset {} is already contained by asset {parent}",
                        edge.dst
                    )));
                }
            }
            AssetRelation::DuplicateOf => {
                let target_is_duplicate: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM asset_edges WHERE src_id = $1 AND rel = 'duplicate_of')
                         OR EXISTS (SELECT 1 FROM asset_edges WHERE dst_id = $2 AND rel = 'duplicate_of')",
                )
                .bind(edge.dst.0)
                .bind(edge.src.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                if target_is_duplicate {
                    return Err(CoreError::Integrity(
                        "duplicate_of must point at a canonical asset".into(),
                    ));
                }
            }
            AssetRelation::References | AssetRelation::DerivedFrom => {}
        }
        sqlx::query("INSERT INTO asset_edges (src_id, dst_id, rel) VALUES ($1, $2, $3)
             ON CONFLICT (src_id, rel, dst_id) DO NOTHING")
            .bind(edge.src.0)
            .bind(edge.dst.0)
            .bind(edge.rel.as_str())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        self.commit(tx, Vec::new()).await
    }

    pub async fn get_asset(&self, id: AssetId) -> CoreResult<Asset> {
        let row = sqlx::query("SELECT * FROM assets WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("asset {id}")))?;
        asset_from_row(&row)
    }

    pub async fn find_asset(&self, kind: AssetKind, identity_key: &str) -> CoreResult<Option<Asset>> {
        let key = arustex_core::domain::asset::normalize_identity(identity_key);
        let row = sqlx::query("SELECT * FROM assets WHERE kind = $1 AND identity_key = $2")
            .bind(kind.as_str())
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        row.as_ref().map(asset_from_row).transpose()
    }

    pub async fn list_assets(&self, query: Option<&str>, page: Page) -> CoreResult<Paged<Asset>> {
        let page = page.clamped();
        let pattern = query
            .filter(|q| !q.trim().is_empty())
            .map(|q| format!("%{}%", q.trim().to_lowercase().replace(['%', '_'], "")));
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM assets WHERE $1::text IS NULL OR lower(label) LIKE $1 OR identity_key LIKE $1",
        )
        .bind(&pattern)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let rows = sqlx::query(
            "SELECT * FROM assets WHERE $1::text IS NULL OR lower(label) LIKE $1 OR identity_key LIKE $1
             ORDER BY id LIMIT $2 OFFSET $3",
        )
        .bind(&pattern)
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let items = rows.iter().map(asset_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Ancestry (nearest parent first, bounded depth) and the direct edges of an asset.
    pub async fn asset_detail(&self, id: AssetId) -> CoreResult<AssetDetail> {
        let asset = self.get_asset(id).await?;
        let rows = sqlx::query(
            "WITH RECURSIVE up(id, depth) AS (
                 SELECT e.src_id, 1 FROM asset_edges e WHERE e.dst_id = $1 AND e.rel = 'contains'
                 UNION ALL
                 SELECT e.src_id, up.depth + 1 FROM asset_edges e JOIN up ON e.dst_id = up.id
                 WHERE e.rel = 'contains' AND up.depth < 64
             )
             SELECT a.* FROM up JOIN assets a ON a.id = up.id ORDER BY up.depth",
        )
        .bind(id.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let ancestry = rows.iter().map(asset_from_row).collect::<CoreResult<_>>()?;
        let edges: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT src_id, dst_id, rel FROM asset_edges WHERE src_id = $1 OR dst_id = $1
             ORDER BY src_id, dst_id LIMIT 500",
        )
        .bind(id.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let edges = edges
            .into_iter()
            .map(|(s, d, r)| {
                Ok(AssetEdge {
                    src: AssetId(s),
                    dst: AssetId(d),
                    rel: r.parse()?,
                })
            })
            .collect::<CoreResult<_>>()?;
        Ok(AssetDetail {
            asset,
            ancestry,
            edges,
        })
    }

    /// Loads the Asset Graph (bounded) and reports invariant violations.
    pub async fn check_asset_graph(&self) -> CoreResult<Vec<AssetGraphIssue>> {
        let assets: Vec<(i64, String)> = sqlx::query_as("SELECT id, kind FROM assets ORDER BY id LIMIT $1")
            .bind(MAX_GRAPH_CHECK_NODES)
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        let edges: Vec<(i64, i64, String)> =
            sqlx::query_as("SELECT src_id, dst_id, rel FROM asset_edges LIMIT $1")
                .bind(MAX_GRAPH_CHECK_NODES * 4)
                .fetch_all(self.pool())
                .await
                .map_err(db_err)?;
        let mut graph = AssetGraph::default();
        for (id, kind) in assets {
            graph.assets.insert(AssetId(id), kind.parse()?);
        }
        for (src, dst, rel) in edges {
            graph.edges.insert(AssetEdge {
                src: AssetId(src),
                dst: AssetId(dst),
                rel: rel.parse()?,
            });
        }
        Ok(graph.check())
    }

    pub async fn asset_count(&self) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM assets")
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        Ok(to_u64(n))
    }

    // -----------------------------------------------------------------------
    // Exploration graph
    // -----------------------------------------------------------------------

    pub async fn exploration_of(&self, task_id: TaskId) -> CoreResult<ExplorationId> {
        let id: i64 = sqlx::query_scalar("SELECT exploration_id FROM tasks WHERE id = $1")
            .bind(task_id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("task {task_id}")))?;
        Ok(ExplorationId(id))
    }

    pub async fn add_node(&self, exploration: ExplorationId, node: &NewNode) -> CoreResult<NodeId> {
        let state = node.effective_state()?;
        let payload = if node.payload.is_null() {
            serde_json::json!({})
        } else {
            node.payload.clone()
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO exploration_nodes (id, exploration_id, kind, state, priority, title, payload, origin,
                                            completed_at)
             VALUES (COALESCE($1, nextval(pg_get_serial_sequence('exploration_nodes', 'id'))),
                     $2, $3, $4, $5, $6, $7, $8, CASE WHEN $9 THEN now() END)
             RETURNING id",
        )
        .bind(node.preserve_id.map(|n| n.0))
        .bind(exploration.0)
        .bind(node.kind.as_str())
        .bind(&state)
        .bind(node.priority)
        .bind(&node.title)
        .bind(payload)
        .bind(&node.origin)
        .bind(node.kind.is_closed(&state))
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(NodeId(id))
    }

    /// Adds an exploration edge, rejecting cycles in `spawns` and `derived_from`.
    pub async fn add_exploration_edge(
        &self,
        exploration: ExplorationId,
        edge: ExplorationEdge,
    ) -> CoreResult<()> {
        let mut tx = self.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(7405623343, $1::int)")
            .bind((exploration.0 % i64::from(i32::MAX)) as i32)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        if matches!(edge.rel, EdgeRelation::Spawns | EdgeRelation::DerivedFrom) {
            let cycle: bool = sqlx::query_scalar(
                "WITH RECURSIVE up(id) AS (
                     SELECT $1::bigint
                     UNION
                     SELECT e.src_id FROM exploration_edges e JOIN up ON e.dst_id = up.id
                     WHERE e.rel = $3 AND e.exploration_id = $4
                 ) SELECT EXISTS (SELECT 1 FROM up WHERE id = $2)",
            )
            .bind(edge.src.0)
            .bind(edge.dst.0)
            .bind(edge.rel.as_str())
            .bind(exploration.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if cycle {
                return Err(CoreError::Integrity(format!(
                    "{} edge {} -> {} would create a cycle",
                    edge.rel, edge.src, edge.dst
                )));
            }
        }
        sqlx::query(
            "INSERT INTO exploration_edges (exploration_id, src_id, dst_id, rel) VALUES ($1, $2, $3, $4)
             ON CONFLICT (exploration_id, src_id, rel, dst_id) DO NOTHING",
        )
        .bind(exploration.0)
        .bind(edge.src.0)
        .bind(edge.dst.0)
        .bind(edge.rel.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        self.commit(tx, Vec::new()).await
    }

    pub async fn anchor(&self, node: NodeId, anchor: Anchor) -> CoreResult<()> {
        let query = match anchor {
            Anchor::Asset(asset) => sqlx::query(
                "INSERT INTO node_asset_anchors (node_id, asset_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(node.0)
            .bind(asset.0),
            Anchor::Record(record) => sqlx::query(
                "INSERT INTO node_record_anchors (node_id, record_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(node.0)
            .bind(record.0),
        };
        query.execute(self.pool()).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn exploration_nodes(
        &self,
        exploration: ExplorationId,
        kind: Option<NodeKind>,
        page: Page,
    ) -> CoreResult<Paged<ExplorationNode>> {
        let page = page.clamped();
        let kind = kind.map(NodeKind::as_str);
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM exploration_nodes WHERE exploration_id = $1 AND ($2::text IS NULL OR kind = $2)",
        )
        .bind(exploration.0)
        .bind(kind)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let rows = sqlx::query(
            "SELECT * FROM exploration_nodes WHERE exploration_id = $1 AND ($2::text IS NULL OR kind = $2)
             ORDER BY id LIMIT $3 OFFSET $4",
        )
        .bind(exploration.0)
        .bind(kind)
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let items = rows.iter().map(node_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Bounded in-memory snapshot of an exploration graph.
    pub async fn load_exploration(&self, exploration: ExplorationId) -> CoreResult<ExplorationSnapshot> {
        let nodes: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT id, kind, state FROM exploration_nodes WHERE exploration_id = $1 ORDER BY id LIMIT $2",
        )
        .bind(exploration.0)
        .bind(MAX_GRAPH_CHECK_NODES)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let edges: Vec<(i64, i64, String)> = sqlx::query_as(
            "SELECT src_id, dst_id, rel FROM exploration_edges WHERE exploration_id = $1 LIMIT $2",
        )
        .bind(exploration.0)
        .bind(MAX_GRAPH_CHECK_NODES * 4)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let record_anchors: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT a.node_id, a.record_id FROM node_record_anchors a
             JOIN exploration_nodes n ON n.id = a.node_id WHERE n.exploration_id = $1",
        )
        .bind(exploration.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let asset_anchors: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT a.node_id, a.asset_id FROM node_asset_anchors a
             JOIN exploration_nodes n ON n.id = a.node_id WHERE n.exploration_id = $1",
        )
        .bind(exploration.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let mut snapshot = ExplorationSnapshot::default();
        for (id, kind, state) in nodes {
            snapshot.nodes.insert(NodeId(id), (kind.parse()?, state));
        }
        for (src, dst, rel) in edges {
            snapshot.edges.insert(ExplorationEdge {
                src: NodeId(src),
                dst: NodeId(dst),
                rel: rel.parse()?,
            });
        }
        let mut anchors: BTreeMap<NodeId, BTreeSet<Anchor>> = BTreeMap::new();
        for (node, record) in record_anchors {
            anchors.entry(NodeId(node)).or_default().insert(Anchor::Record(RecordId(record)));
        }
        for (node, asset) in asset_anchors {
            anchors.entry(NodeId(node)).or_default().insert(Anchor::Asset(AssetId(asset)));
        }
        snapshot.anchors = anchors;
        Ok(snapshot)
    }

    pub async fn check_exploration(&self, task_id: TaskId) -> CoreResult<Vec<ExplorationIssue>> {
        let exploration = self.exploration_of(task_id).await?;
        Ok(self.load_exploration(exploration).await?.check())
    }

    /// Provenance-bearing generated assets are labelled as derived.
    pub fn derived_provenance(source: &str) -> Provenance {
        Provenance {
            origin: Origin::Derived,
            source: source.into(),
            locator: None,
            session_id: None,
        }
    }
}
