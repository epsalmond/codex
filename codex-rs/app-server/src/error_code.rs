use codex_app_server_protocol::JSONRPCErrorError;
use codex_core::NotSubmittedReason;

pub(crate) const INVALID_REQUEST_ERROR_CODE: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND_ERROR_CODE: i64 = -32601;
pub const INVALID_PARAMS_ERROR_CODE: i64 = -32602;
pub(crate) const INTERNAL_ERROR_CODE: i64 = -32603;
pub(crate) const OVERLOADED_ERROR_CODE: i64 = -32001;
pub const INPUT_TOO_LARGE_ERROR_CODE: &str = "input_too_large";

pub(crate) fn server_draining_error() -> JSONRPCErrorError {
    invalid_request("Server is draining; retry after reconnecting")
}

/// The retryable error for a turn start Core declined only because the host cannot admit new
/// turns right now, or `None` for any other reason. A full root-turn capacity uses the overload
/// code a full request queue also returns.
pub(crate) fn retryable_turn_start_error(reason: &NotSubmittedReason) -> Option<JSONRPCErrorError> {
    match reason {
        NotSubmittedReason::ServerDraining => Some(server_draining_error()),
        NotSubmittedReason::RootTurnCapacityReached => Some(error(
            OVERLOADED_ERROR_CODE,
            "Server is at its root turn capacity; retry later.",
        )),
        _ => None,
    }
}

/// The retryable error for `turn/start` input Core declined, or `None` for a reason that is not
/// retryable. Besides admission refusals, input can lose its slot to other turns several times
/// in a row while it starts; retrying then steers into or starts after them.
pub(crate) fn retryable_start_or_steer_error(
    reason: &NotSubmittedReason,
) -> Option<JSONRPCErrorError> {
    match reason {
        NotSubmittedReason::NotIdle => Some(error(
            OVERLOADED_ERROR_CODE,
            "Other turns kept taking this thread while the turn started; retry.",
        )),
        reason => retryable_turn_start_error(reason),
    }
}

pub(crate) fn invalid_request(message: impl Into<String>) -> JSONRPCErrorError {
    error(INVALID_REQUEST_ERROR_CODE, message)
}

pub(crate) fn method_not_found(message: impl Into<String>) -> JSONRPCErrorError {
    error(METHOD_NOT_FOUND_ERROR_CODE, message)
}

pub(crate) fn invalid_params(message: impl Into<String>) -> JSONRPCErrorError {
    error(INVALID_PARAMS_ERROR_CODE, message)
}

pub(crate) fn internal_error(message: impl Into<String>) -> JSONRPCErrorError {
    error(INTERNAL_ERROR_CODE, message)
}

fn error(code: i64, message: impl Into<String>) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code,
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn admission_refusals_map_to_retryable_codes() {
        let codes = [
            NotSubmittedReason::ServerDraining,
            NotSubmittedReason::RootTurnCapacityReached,
            NotSubmittedReason::NotIdle,
        ]
        .map(|reason| retryable_turn_start_error(&reason).map(|error| error.code));
        assert_eq!(
            codes,
            [
                Some(INVALID_REQUEST_ERROR_CODE),
                Some(OVERLOADED_ERROR_CODE),
                None
            ]
        );
    }

    #[test]
    fn start_or_steer_that_kept_losing_its_slot_maps_to_a_retryable_code() {
        let codes = [
            NotSubmittedReason::NotIdle,
            NotSubmittedReason::RootTurnCapacityReached,
            NotSubmittedReason::EmptyInput,
        ]
        .map(|reason| retryable_start_or_steer_error(&reason).map(|error| error.code));
        assert_eq!(
            codes,
            [
                Some(OVERLOADED_ERROR_CODE),
                Some(OVERLOADED_ERROR_CODE),
                None
            ]
        );
    }
}
