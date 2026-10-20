use crate::models::{ITEM_COLS, ItemSummary, item_from_row};
use serde::Serialize;
use sqlx::{AssertSqlSafe, PgPool, Row};
use std::collections::HashMap;

/// One node of an item's build tree, annotated with a user's progress.
#[derive(Serialize, Clone, Debug)]
pub struct ProgressNode {
    pub item: ItemSummary,
    /// Total quantity needed for the root, counts multiplied down the tree.
    pub needed: i64,
    pub have: i64,
    pub obtained: bool,
    /// `obtained || have >= needed`.
    pub covered: bool,
    pub children: Vec<ProgressNode>,
}

impl ProgressNode {
    /// Share of direct children covered; 100 when this node is covered or a leaf.
    pub fn progress_pct(&self) -> f64 {
        if self.covered || self.children.is_empty() {
            return 100.0;
        }
        let done = self.children.iter().filter(|c| c.covered).count();
        done as f64 * 100.0 / self.children.len() as f64
    }
}

/// Guards against cyclic wiki recipes.
const MAX_DEPTH: usize = 8;

type NodeData = (ItemSummary, bool, i64);

pub async fn load_tree(
    pool: &PgPool,
    user_id: i64,
    root: i64,
) -> sqlx::Result<Option<ProgressNode>> {
    let sql = format!(
        r#"
        WITH RECURSIVE reach(id, depth) AS (
            SELECT $2::bigint, 0
            UNION
            SELECT r.ingredient_item_id, reach.depth + 1
            FROM recipes r JOIN reach ON r.result_item_id = reach.id
            WHERE reach.depth < {MAX_DEPTH}
        )
        SELECT DISTINCT ON (i.id) {ITEM_COLS},
               COALESCE(cp.obtained, false) AS obtained, COALESCE(inv.count, 0) AS have
        FROM reach JOIN items i ON i.id = reach.id
        LEFT JOIN component_progress cp ON cp.user_id = $1 AND cp.item_id = i.id
        LEFT JOIN inventory inv ON inv.user_id = $1 AND inv.item_id = i.id"#
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(user_id)
        .bind(root)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let nodes: HashMap<i64, NodeData> = rows
        .iter()
        .map(|r| {
            let it = item_from_row(r);
            (it.id, (it, r.get("obtained"), r.get("have")))
        })
        .collect();
    let ids: Vec<i64> = nodes.keys().copied().collect();
    let edges: Vec<(i64, i64, i32)> = sqlx::query_as(
        "SELECT result_item_id, ingredient_item_id, count FROM recipes
         WHERE result_item_id = ANY($1) ORDER BY ingredient_item_id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let mut kids: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for (p, c, n) in edges {
        kids.entry(p).or_default().push((c, n as i64));
    }
    Ok(Some(build(root, 1, 0, &nodes, &kids)))
}

fn build(
    id: i64,
    needed: i64,
    depth: usize,
    nodes: &HashMap<i64, NodeData>,
    kids: &HashMap<i64, Vec<(i64, i64)>>,
) -> ProgressNode {
    let (item, obtained, have) = nodes[&id].clone();
    let children = if depth >= MAX_DEPTH {
        vec![]
    } else {
        kids.get(&id)
            .into_iter()
            .flatten()
            .filter(|(c, _)| nodes.contains_key(c))
            .map(|(c, n)| build(*c, needed.saturating_mul(*n), depth + 1, nodes, kids))
            .collect()
    };
    ProgressNode {
        covered: obtained || have >= needed,
        item,
        needed,
        have,
        obtained,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::load_tree;
    use crate::db::test_pool;

    /// Saryn Prime ← {Chassis ×1, Neuroptics ×1, Orokin Cell ×1}; Chassis ← Rubedo ×1200; Neuroptics ← Rubedo ×750
    async fn seed(pool: &sqlx::PgPool) -> i64 {
        sqlx::raw_sql(
            r#"
          INSERT INTO items (id, wiki_name, category) VALUES
            (1,'Saryn Prime','warframe'),(2,'Saryn Prime Chassis','component'),
            (3,'Saryn Prime Neuroptics','component'),(4,'Orokin Cell','resource'),(5,'Rubedo','resource');
          INSERT INTO recipes VALUES (1,2,1),(1,3,1),(1,4,1),(2,5,1200),(3,5,750);
        "#,
        )
        .execute(pool)
        .await
        .unwrap();
        1
    }

    #[tokio::test]
    async fn builds_tree_with_multiplied_counts() {
        let pool = test_pool().await;
        let root = seed(&pool).await;
        let t = load_tree(&pool, 1, root).await.unwrap().unwrap();
        assert_eq!(t.children.len(), 3);
        let chassis = t
            .children
            .iter()
            .find(|c| c.item.name == "Saryn Prime Chassis")
            .unwrap();
        assert_eq!(chassis.children[0].needed, 1200);
        assert!(!t.covered);
    }

    #[tokio::test]
    async fn obtained_or_inventory_marks_covered() {
        let pool = test_pool().await;
        let root = seed(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO component_progress VALUES (1,2,true);
             INSERT INTO inventory (user_id,item_id,count) VALUES (1,4,1);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let t = load_tree(&pool, 1, root).await.unwrap().unwrap();
        let by = |n: &str| t.children.iter().find(|c| c.item.name == n).unwrap();
        assert!(by("Saryn Prime Chassis").covered && by("Saryn Prime Chassis").obtained);
        assert!(by("Orokin Cell").covered);
        assert!(!by("Saryn Prime Neuroptics").covered);
        assert!((t.progress_pct() - 66.666).abs() < 0.1);
    }

    #[tokio::test]
    async fn unknown_root_is_none() {
        let pool = test_pool().await;
        assert!(load_tree(&pool, 1, 999).await.unwrap().is_none());
    }
}
