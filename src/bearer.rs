use axum::{
    body::HttpBody,
    http::{HeaderValue, Request, Response, StatusCode},
};
use constant_time_eq::constant_time_eq;
use regex::RegexSet;
use std::marker::PhantomData;
use tower_http::auth::AuthorizeRequest;

pub struct BearerAuth<ResBody> {
    api_token: HeaderValue,
    public_paths: RegexSet,
    _phantom: PhantomData<ResBody>,
}

// Derive clone did not work
// It did too much introspection into the generic type, which actually does not need
// to implement Clone
impl<ResBody> Clone for BearerAuth<ResBody> {
    fn clone(&self) -> Self {
        Self {
            api_token: self.api_token.clone(),
            public_paths: self.public_paths.clone(),
            _phantom: self._phantom,
        }
    }
}

impl<ResBody> BearerAuth<ResBody> {
    pub fn new(api_token: HeaderValue, public_paths: RegexSet) -> Self {
        Self {
            api_token,
            public_paths,
            _phantom: Default::default(),
        }
    }
}

impl<ReqBody, ResBody> AuthorizeRequest<ReqBody> for BearerAuth<ResBody>
where
    ReqBody: HttpBody,
    ResBody: HttpBody + Default,
{
    type ResponseBody = ResBody;

    fn authorize(
        &mut self,
        request: &mut Request<ReqBody>,
    ) -> Result<(), Response<Self::ResponseBody>> {
        macro_rules! unauthorized {
            () => {
                return Err(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Default::default())
                    .unwrap())
            };
        }
        if self.public_paths.is_match(request.uri().path()) {
            return Ok(());
        }

        match request.headers().get("Authorization") {
            Some(header) => {
                let header = match header.to_str() {
                    Ok(x) => x,
                    Err(_) => unauthorized!(),
                };

                if !header.starts_with("Bearer ") {
                    unauthorized!()
                }

                let token = header.split_at(7).1;

                if constant_time_eq(token.as_bytes(), self.api_token.as_bytes()) {
                    Ok(())
                } else {
                    unauthorized!()
                }
            }
            None => {
                if let Some(query) = request.uri().query() {
                    if query.contains(&format!(
                        "api_token={}",
                        self.api_token.to_str().expect("API Token to be utf-8")
                    )) {
                        return Ok(());
                    }
                }
                unauthorized!()
            }
        }
    }
}
