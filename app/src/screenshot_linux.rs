//! Interactive screenshots through the desktop's XDG Screenshot portal.

use ashpd::desktop::screenshot::Screenshot;
use ashpd::desktop::ResponseError;
use jiffshot_doc::Document;

pub(super) async fn capture_interactive() -> Result<Option<Document>, String> {
    let response = Screenshot::request()
        .interactive(true)
        .modal(true)
        .send()
        .await
        .and_then(|request| request.response());
    let Some(screenshot) = classify_response(response)? else {
        return Ok(None);
    };
    let path = screenshot
        .uri()
        .to_file_path()
        .map_err(|_| "the desktop returned a non-local screenshot URI".to_owned())?;
    let document = jiffshot_doc::io::open(&path)
        .map_err(|error| format!("could not open the desktop screenshot: {error}"))?;
    Ok(Some(document))
}

fn classify_response<T>(response: Result<T, ashpd::Error>) -> Result<Option<T>, String> {
    match response {
        Ok(value) => Ok(Some(value)),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(None),
        Err(error) => Err(format!("desktop screenshot failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_response_distinguishes_cancellation_and_failure() {
        assert_eq!(classify_response(Ok(7)), Ok(Some(7)));
        assert_eq!(
            classify_response::<u8>(Err(ashpd::Error::Response(ResponseError::Cancelled))),
            Ok(None)
        );
        let error = classify_response::<u8>(Err(ashpd::Error::Response(ResponseError::Other)))
            .expect_err("a failed portal response must not be reported as cancellation");
        assert!(error.contains("desktop screenshot failed"), "{error}");
    }
}
