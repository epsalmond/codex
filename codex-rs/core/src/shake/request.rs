use codex_api::ResponsesApiRequest;
use codex_history::ShakeHistoryState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use sha1::Digest;
use sha1::Sha1;

#[derive(Debug, Clone)]
pub(crate) struct ShakeRequestCheck {
    pub(crate) history_state: ShakeHistoryState,
    pub(crate) prefix_items: Vec<ResponseItem>,
    pub(crate) base_instructions: String,
    pub(crate) provider_id: String,
    pub(crate) input_modalities: Vec<InputModality>,
    pub(crate) use_responses_lite: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ShakeRequestCacheEpoch {
    history_epoch_id: String,
    watermark: u64,
    dynamic_identity: String,
    prefix_len: usize,
    prefix_digest: String,
}

pub(crate) fn validate_shake_request_prefix_epoch(
    cache_epoch: &mut Option<ShakeRequestCacheEpoch>,
    request: &ResponsesApiRequest,
    check: Option<&ShakeRequestCheck>,
) -> Result<()> {
    let Some(check) = check else {
        return Ok(());
    };
    let prefix = request
        .input
        .get(..check.prefix_items.len())
        .ok_or_else(|| {
            CodexErr::Stream(
                "Shake sealed prefix exceeds the prepared Responses request".to_string(),
            )
        })?;
    if super::watermark::canonical_json_bytes(prefix)?
        != super::watermark::canonical_json_bytes(&check.prefix_items)?
    {
        return Err(CodexErr::Stream(
            "Shake sealed Responses prefix changed while preparing the request".to_string(),
        ));
    }
    let prefix_digest = shake_wire_prefix_digest(prefix)?;
    let mut dynamic_request = request.clone();
    dynamic_request.input.clear();
    dynamic_request.client_metadata = None;
    let dynamic_identity = super::watermark::canonical_json_bytes(&(
        dynamic_request,
        &check.base_instructions,
        &check.provider_id,
        &check.input_modalities,
        check.use_responses_lite,
    ))
    .map(|bytes| format!("{:x}", Sha1::digest(bytes)))?;

    if let Some(previous) = cache_epoch
        && previous.history_epoch_id == check.history_state.epoch_id
        && previous.dynamic_identity == dynamic_identity
    {
        let previous_prefix = request.input.get(..previous.prefix_len).ok_or_else(|| {
            CodexErr::Stream("Shake sealed Responses prefix became shorter".to_string())
        })?;
        let watermark_advanced = check.history_state.watermark > previous.watermark;
        if check.history_state.watermark < previous.watermark
            || prefix.len() < previous.prefix_len
            || shake_wire_prefix_digest(previous_prefix)? != previous.prefix_digest
            || (!watermark_advanced && prefix.len() != previous.prefix_len)
        {
            return Err(CodexErr::Stream(
                "Shake sealed Responses prefix changed within a request cache epoch".to_string(),
            ));
        }
    }

    *cache_epoch = Some(ShakeRequestCacheEpoch {
        history_epoch_id: check.history_state.epoch_id.clone(),
        watermark: check.history_state.watermark,
        dynamic_identity,
        prefix_len: prefix.len(),
        prefix_digest,
    });
    Ok(())
}

pub(crate) fn validate_shake_wire_prefix(
    input: &[ResponseItem],
    check: Option<&ShakeRequestCheck>,
) -> Result<()> {
    let Some(check) = check else {
        return Ok(());
    };
    if !shake_wire_prefix_matches(input, Some(check))? {
        return Err(CodexErr::Stream(
            "Shake sealed Responses prefix changed while bounding the WebSocket request"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn shake_wire_prefix_matches(
    input: &[ResponseItem],
    check: Option<&ShakeRequestCheck>,
) -> Result<bool> {
    let Some(check) = check else {
        return Ok(false);
    };
    let Some(prefix) = input.get(..check.prefix_items.len()) else {
        return Ok(false);
    };
    Ok(super::watermark::canonical_json_bytes(prefix)?
        == super::watermark::canonical_json_bytes(&check.prefix_items)?)
}

fn shake_wire_prefix_digest(prefix: &[ResponseItem]) -> Result<String> {
    let bytes = super::watermark::canonical_json_bytes(prefix)?;
    Ok(format!("{:x}", Sha1::digest(bytes)))
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
