/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use content_security_policy::Destination;
use embedder_traits::{GenericEmbedderProxy, WebResourceRequest, WebResourceResponseMsg};
use log::error;
use net_traits::NetworkError;
use net_traits::http_status::HttpStatus;
use ipc_channel::ipc;
use net_traits::request::{BodyChunkRequest, BodyChunkResponse, Request};
use net_traits::response::{Response, ResponseBody};

use crate::embedder::NetToEmbedderMsg;
use crate::fetch::methods::FetchContext;

#[derive(Clone)]
pub struct RequestInterceptor {
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
}

impl RequestInterceptor {
    pub fn new(embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>) -> RequestInterceptor {
        RequestInterceptor { embedder_proxy }
    }

    /// The whole request body, when the request is for the host named by the
    /// `network_intercept_body_host` preference (the embedder serves it itself). The body stream
    /// is consumed, so this is only done for a host the embedder is expected to intercept.
    fn read_body_if_wanted(request: &Request) -> Option<Vec<u8>> {
        let wanted = servo_config::pref!(network_intercept_body_host);
        if wanted.is_empty() || request.url().host_str() != Some(wanted.as_str()) {
            return None;
        }
        let stream = request.body.as_ref()?.clone_stream();
        let mut guard = stream.lock();
        let chunk_requester = guard.as_mut()?;
        let (body_chan, body_port) = ipc::channel().ok()?;
        chunk_requester
            .send(BodyChunkRequest::Connect(body_chan))
            .ok()?;
        let mut out = Vec::new();
        loop {
            chunk_requester.send(BodyChunkRequest::Chunk).ok()?;
            match body_port.recv().ok()? {
                BodyChunkResponse::Chunk(bytes) => out.extend_from_slice(&bytes),
                BodyChunkResponse::Done => break,
                BodyChunkResponse::Error => return None,
            }
        }
        Some(out)
    }

    pub async fn intercept_request(
        &self,
        request: &mut Request,
        response: &mut Option<Response>,
        context: &FetchContext,
    ) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let is_for_main_frame = matches!(request.destination, Destination::Document);
        let web_resource_request = WebResourceRequest {
            method: request.method.clone(),
            url: request.url().into_url(),
            headers: request.headers.clone(),
            destination: request.destination,
            referrer_url: request.referrer.to_url().map(|url| url.as_url().clone()),
            is_for_main_frame,
            is_redirect: request.redirect_count > 0,
            body: Self::read_body_if_wanted(request),
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
