use crate::args::{flag_bool, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::{reconcile, ReconcileOpts};
use serde_json::{json, Map, Value};

pub fn reindex_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let force = flag_bool(&parsed.flags, "force");
    let with_embeddings = !flag_bool(&parsed.flags, "no-embed");
    let db = ctx.db()?;
    let res = reconcile(
        db,
        &root,
        ReconcileOpts {
            force,
            with_embeddings: Some(with_embeddings),
        },
    );

    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));
    out.insert("vault".to_string(), json!(root));
    out.insert("scanned".to_string(), json!(res.scanned));
    out.insert("indexed".to_string(), json!(res.indexed));
    out.insert("updated".to_string(), json!(res.updated));
    out.insert("removed".to_string(), json!(res.removed));
    out.insert("unchanged".to_string(), json!(res.unchanged));
    out.insert("embedded".to_string(), json!(res.embedded));
    out.insert(
        "embeddingsAvailable".to_string(),
        json!(res.embeddings_available),
    );
    if with_embeddings && !res.embeddings_available {
        out.insert(
            "note".to_string(),
            json!(
                "Embeddings unavailable (offline or disabled) — search falls back to FTS5 + graph"
            ),
        );
    }
    Ok(Value::Object(out))
}
