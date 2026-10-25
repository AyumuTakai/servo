/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use content_security_policy::Destination;
use embedder_traits::{GenericEmbedderProxy, WebResourceRequest, WebResourceResponseMsg};
use log::error;
use net_traits::NetworkError;
use net_traits::http_status::HttpStatus;
use ipc_channel::ipc;
use net_traits::request::{BodyChunkRequest, BodyChunkResponse, Origin, Request};
use net_traits::response::{Response, ResponseBody};

use crate::embedder::NetToEmbedderMsg;
use crate::fetch::methods::FetchContext;

/// How long to wait for each chunk of a request body that is read for the embedder.
const BODY_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
pub struct RequestInterceptor {
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
}

impl RequestInterceptor {
    pub fn new(embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>) -> RequestInterceptor {
        RequestInterceptor { embedder_proxy }
    }

    /// The whole request body, when the request is for the host named by the
    /// `network_intercept_body_host` preference (the embedder serves it itself) **and** it comes
    /// from a document of that same origin. The body stream is consumed, so this is only done for
    /// a host the embedder is expected to intercept. Requests from other origins (any web page can
    /// send one to that host) get no body. `Err`: the body is larger than
    /// `network_intercept_body_max_bytes`, or a chunk did not arrive in time.
    fn read_body_if_wanted(request: &Request) -> Result<Option<Vec<u8>>, NetworkError> {
        let prefs = servo_config::prefs::get();
        let wanted = &prefs.network_intercept_body_host;
        if wanted.is_empty() || request.current_url().host_str() != Some(wanted.as_str()) {
            return Ok(None);
        }
        let same_origin =
            matches!(&request.origin, Origin::Origin(origin) if *origin == request.current_url().origin());
        if !same_origin {
            return Ok(None);
        }
        let max = prefs.network_intercept_body_max_bytes;
        let Some(body) = request.body.as_ref() else {
            return Ok(None);
        };
        let too_large = || {
            NetworkError::ResourceLoadError(format!("request body is larger than {max} bytes"))
        };
        if body.len().is_some_and(|len| len as u64 > max) {
            return Err(too_large());
        }
        let stream = body.clone_stream();
        let mut guard = stream.lock();
        let Some(chunk_requester) = guard.as_mut() else {
            return Ok(None);
        };
        let Ok((body_chan, body_port)) = ipc::channel() else {
            return Ok(None);
        };
        if chunk_requester
            .send(BodyChunkRequest::Connect(body_chan))
            .is_err()
        {
            return Ok(None);
        }
        let mut out = Vec::new();
        loop {
            if chunk_requester.send(BodyChunkRequest::Chunk).is_err() {
                return Ok(None);
            }
            // A page that never finishes its body stream must not hold this thread forever.
            match body_port.try_recv_timeout(BODY_CHUNK_TIMEOUT) {
                Ok(BodyChunkResponse::Chunk(bytes)) => {
                    if (out.len() + bytes.len()) as u64 > max {
                        return Err(too_large());
                    }
                    out.extend_from_slice(&bytes)
                },
                Ok(BodyChunkResponse::Done) => break,
                Ok(BodyChunkResponse::Error) => return Ok(None),
                Err(_) => {
                    return Err(NetworkError::ResourceLoadError(
                        "the request body did not arrive in time".to_owned(),
                    ));
                },
            }
        }
        Ok(Some(out))
    }

    pub async fn intercept_request(
        &self,
        request: &mut Request,
        response: &mut Option<Response>,
        context: &FetchContext,
    ) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let is_for_main_frame = matches!(request.destination, Destination::Document);
        let body = match Self::read_body_if_wanted(request) {
            Ok(body) => body,
            Err(error) => {
                *response = Some(Response::network_error(error));
                return;
            },
        };
        let web_resource_request = WebResourceRequest {
            method: request.method.clone(),
            // The URL being fetched now: after a redirect, the embedder must see where the
            // request goes (the first URL of the list is where it started).
            url: request.current_url().into_url(),
            headers: request.headers.clone(),
            destination: request.destination,
            referrer_url: request.referrer.to_url().map(|url| url.as_url().clone()),
            is_for_main_frame,
            is_redirect: request.redirect_count > 0,
            body,
            origin: match &request.origin {
                Origin::Origin(origin) => Some(origin.ascii_serialization().into_owned()),
                Origin::Client => None,
            },
        };

        self.embedder_proxy
            .send(NetToEmbedderMsg::WebResourceRequested(
                request.target_webview_id,
                web_resource_request,
                sender,
            ));

        // TODO: use done_chan and run in CoreResourceThreadPool.
        let mut accumulated_body = Vec::new();
        while let Some(message) = receiver.recv().await {
            match message {
                WebResourceResponseMsg::Start(webresource_response) => {
                    let timing = context.timing.inner().clone();
                    let mut response_override =
                        Response::new(webresource_response.url.into(), timing);
                    response_override.headers = webresource_response.headers;
                    response_override.status = HttpStatus::new(
                        webresource_response.status_code,
                        webresource_response.status_message,
                    );
                    *response = Some(response_override);
                },
                WebResourceResponseMsg::SendBodyData(data) => {
                    accumulated_body.push(data);
                },
                WebResourceResponseMsg::FinishLoad => {
                    if accumulated_body.is_empty() {
                        break;
                    }
                    let Some(response) = response.as_mut() else {
                        error!("Received unexpected FinishLoad message");
                        break;
                    };
                    *response.body.lock() =
                        ResponseBody::Done(accumulated_body.into_iter().flatten().collect());
                    break;
                },
                WebResourceResponseMsg::CancelLoad => {
                    *response = Some(Response::network_error(NetworkError::LoadCancelled));
                    break;
                },
                WebResourceResponseMsg::DoNotIntercept => break,
            }
        }
    }
}
