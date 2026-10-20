use serde::Serialize;
use sqlx::{Row, postgres::PgRow};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ItemSummary {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub subcategory: Option<String>,
    pub is_prime: bool,
    pub is_vaulted: Option<bool>,
    pub image_url: Option<String>,
    pub wiki_url: Option<String>,
}

/// Columns for `FROM items i`; pair with `item_from_row`.
pub const ITEM_COLS: &str =
    "i.id, i.wiki_name, i.category, i.subcategory, i.is_prime, i.is_vaulted, i.image, i.wiki_link";

pub fn item_from_row(r: &PgRow) -> ItemSummary {
    let image: Option<String> = r.get("image");
    let link: Option<String> = r.get("wiki_link");
    ItemSummary {
        id: r.get("id"),
        name: r.get("wiki_name"),
        category: r.get("category"),
        subcategory: r.get("subcategory"),
        is_prime: r.get("is_prime"),
        is_vaulted: r.get("is_vaulted"),
        image_url: image.map(|f| {
            format!(
                "https://wiki.warframe.com/w/Special:FilePath/{}",
                f.replace(' ', "_")
            )
        }),
        wiki_url: link.map(|l| format!("https://wiki.warframe.com/w/{}", l.replace(' ', "_"))),
    }
}
