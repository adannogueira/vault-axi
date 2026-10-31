use crate::args::{flag_bool, flag_string, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::{reconcile, ReconcileOpts};
use crate::sync::{handle_mfa, login_hosted, register_hosted, run_full_sync};
use serde_json::{json, Value};

pub fn sync_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let only_pull = flag_bool(&parsed.flags, "pull");
    let only_push = flag_bool(&parsed.flags, "push");
    let (do_pull, do_push) = match (only_pull, only_push) {
        (true, false) => (true, false),
        (false, true) => (false, true),
        _ => (true, true),
    };

    let db = ctx.db()?;
    let summary = run_full_sync(db, &root, do_pull, do_push)?;
    let rec = reconcile(
        db,
        &root,
        ReconcileOpts {
            force: false,
            with_embeddings: Some(false),
        },
    );

    Ok(json!({
        "ok": true,
        "backend": summary.backend,
        "pulled": summary.pulled,
        "pushed": summary.pushed,
        "deletedLocal": summary.deleted_local,
        "deletedRemote": summary.deleted_remote,
        "indexed": rec.indexed,
        "updated": rec.updated,
        "message": summary.message
    }))
}

pub fn register_command(args: &[String], _ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);
    let email = flag_string(&parsed.flags, "email");
    let password = flag_string(&parsed.flags, "password");
    let (Some(em), Some(pw)) = (email, password) else {
        return Err(AxiError::new(
            "Missing --email or --password",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault register --email <you@example.com> --password <password> [--vault-id <id>]"
                    .to_string(),
            ],
        ));
    };

    let vault_id = flag_string(&parsed.flags, "vault-id");

    register_hosted(&em, &pw, vault_id.as_deref())
}

pub fn login_command(args: &[String], _ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);

    if flag_bool(&parsed.flags, "mfa-enroll") {
        return handle_mfa("enroll", None);
    }
    if let Some(verify_code) = flag_string(&parsed.flags, "mfa-verify") {
        return handle_mfa("verify", Some(&verify_code));
    }

    let email = flag_string(&parsed.flags, "email");
    let password = flag_string(&parsed.flags, "password");
    let mfa_code = flag_string(&parsed.flags, "mfa-code");
    let token = flag_string(&parsed.flags, "token");
    let vault_id = flag_string(&parsed.flags, "vault-id");

    login_hosted(
        email.as_deref(),
        password.as_deref(),
        mfa_code.as_deref(),
        token.as_deref(),
        vault_id.as_deref(),
    )
}
