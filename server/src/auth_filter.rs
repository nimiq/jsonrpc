//! HTTP Authentication Filters

use headers::{authorization::Basic, Authorization, HeaderMap, HeaderMapExt};
use http::Response;
use warp::{header, http::StatusCode, reject, reject::Rejection, Filter, Reply};

/// Custom reject reason when the authorization header is wrong or is not found.
#[derive(Debug)]
pub struct Unauthorized {
    pub(crate) realm: String,
}

impl reject::Reject for Unauthorized {}

/// Handles a `Unauthorized` rejection to return a 401 Unauthorized status.
pub(crate) async fn handle_auth_rejection(
    err: Rejection,
) -> Result<impl Reply, std::convert::Infallible> {
    let response = if err.is_not_found() {
        Response::builder().status(StatusCode::NOT_FOUND).body("")
    } else if let Some(error) = err.find::<Unauthorized>() {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                "WWW-Authenticate",
                format!("Basic realm = \"{}\"", error.realm),
            )
            .body("")
    } else {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("")
    };
    Ok(response)
}

/// Creates a `Filter` to the HTTP Basic Authentication header.
/// If none was sent by the client, this filter will reject any request.
///
/// The `handle_rejection` recover filter can be used along with this filter to
/// properly return a 401 Unauthorized status whenever the header is not found.
///
pub(crate) fn basic_auth_filter(
    realm: &str,
) -> impl Filter<Extract = (Authorization<Basic>,), Error = Rejection> + '_ {
    header::headers_cloned().and_then(move |headers: HeaderMap| async move {
        headers.typed_get::<Authorization<Basic>>().ok_or_else(|| {
            reject::custom(Unauthorized {
                realm: realm.to_string(),
            })
        })
    })
}
