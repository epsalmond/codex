//! Routes device verification directly to the app, outside automated approval policy.

use super::*;

pub(super) async fn route(
    router: ElicitationRequestRouter,
    events: Option<Sender<Event>>,
    authority: Option<ElicitationAuthority>,
    server_key: McpServerKey,
    request: ElicitationRequest,
) -> Result<ElicitationResponse> {
    let plugin_service = authority.as_ref().is_some_and(|authority| {
        authority
            .config
            .mcp_server_catalog
            .server(&server_key)
            .is_some_and(|server| {
                server
                    .source()
                    .is_host_owned_apps(server_key.name(), server.config())
            })
    });
    let Some((events, authority)) = events.zip(authority).filter(|_| plugin_service) else {
        return Ok(approval_unavailable());
    };
    let ElicitationRequest::UserVerification { meta, .. } = &request else {
        return Ok(approval_unavailable());
    };
    if let Some(reviewer) = &authority.reviewer
        && let Some(reason) = reviewer
            .idle_refusal(
                &server_key,
                elicitation_call_id(&server_key, meta.as_ref().and_then(Value::as_object)),
            )
            .await
    {
        tracing::debug!("{reason}");
        return Ok(approval_unavailable());
    }
    router
        .request_user_interaction(Some(events), &authority, server_key, request)
        .await
}

fn approval_unavailable() -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Cancel,
        content: None,
        meta: Some(codex_rmcp_client::UserVerificationReason::ApprovalUnavailable.into_meta()),
    }
}
