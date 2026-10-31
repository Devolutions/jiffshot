pub fn combine_session_results<T>(
    result: Result<T, String>,
    closed: Result<(), String>,
) -> Result<T, String> {
    match (result, closed) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(close_error)) => Err(format!("{error}; {close_error}")),
    }
}
