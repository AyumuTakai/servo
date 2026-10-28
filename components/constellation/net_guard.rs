/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! (Kyousei) コンテンツプロセスから、親のネットワークのスレッドへ向かう要求の検問。
//!
//! マルチプロセスでは、子(script スレッド)が、親の net スレッドへ、直接メッセージを送る。乗っ取られた子が、
//! (1)ほかのサイトの Cookie を読み書きする(2)送り主(origin)を偽って `fetch` し、ログイン中のサイトの応答を読む、のを防ぐため、
//! 子ごとに、親の側に中継のスレッドを置き、子が受け持つサイト(`Sites`)に当てはまらない要求を捨てる。
//! 受け持つサイトは、constellation が、その子に載せるページの URL(最初の URL・最終の URL)から足す。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use log::warn;
use embedder_traits::EmbedderMsg;
use net_traits::request::{Destination, Origin};
use servo_base::id::WebViewId;
use net_traits::{CoreResourceMsg, CoreResourceThread};
use servo_base::generic_channel;
use servo_url::{ImmutableOrigin, ServoUrl};

/// その子が受け持つサイトの鍵の集合(`site_key`)。
pub(crate) type Sites = Arc<Mutex<HashSet<String>>>;

/// (Kyousei) その子(コンテンツプロセス)が、パイプラインを持つ面。子の申告する面(`WebViewId`)が本当にその子のものかを、親の側で検査する。
/// 数は、その面に載っているパイプラインの数(最上位のものは `top` にも数える)。constellation が、パイプラインの増減で更新する。
#[derive(Default)]
pub(crate) struct OwnedViews {
    webviews: std::collections::HashMap<WebViewId, usize>,
    top: std::collections::HashMap<WebViewId, usize>,
}

pub(crate) type Owned = Arc<Mutex<OwnedViews>>;

impl OwnedViews {
    pub(crate) fn add(&mut self, webview: WebViewId, top_level: bool) {
        *self.webviews.entry(webview).or_default() += 1;
        if top_level {
            *self.top.entry(webview).or_default() += 1;
        }
    }

    pub(crate) fn remove(&mut self, webview: WebViewId, top_level: bool) {
        fn dec(m: &mut std::collections::HashMap<WebViewId, usize>, k: WebViewId) {
            if let Some(n) = m.get_mut(&k) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&k);
                }
            }
        }
        dec(&mut self.webviews, webview);
        if top_level {
            dec(&mut self.top, webview);
        }
    }

    pub(crate) fn has(&self, webview: WebViewId) -> bool {
        self.webviews.contains_key(&webview)
    }

    pub(crate) fn has_top_level(&self, webview: WebViewId) -> bool {
        self.top.contains_key(&webview)
    }
}

/// 子から埋め込み側(コンポジタ)への通知を通してよいか。面を持つメッセージは、その子がパイプラインを持つ面のものだけ。
/// クリップボードの読み取りと、竟成OS のシェルへのメッセージ(`HostChannelMessage`)は、さらに、その面の最上位のページを持つ子だけ
/// (別のサイトの iframe の子が、親の面のふりをしないように)。面を持たない開発ツールの通知は、使っていないので捨てる。
pub(crate) fn embedder_msg_allowed(owned: &Owned, msg: &EmbedderMsg) -> bool {
    use EmbedderMsg::*;
    let o = owned.lock().unwrap();
    match msg {
        HostChannelMessage(w, ..) | GetClipboardText(w, _) => o.has_top_level(*w),
        DownloadEvent(w, _) |
        Status(w, _) |
        ChangePageTitle(w, _) |
        MoveTo(w, _) |
        ResizeTo(w, _) |
        ShowSimpleDialog(w, _) |
        AllowProtocolHandlerRequest(w, ..) |
        AllowUnload(w, _) |
        ClearClipboard(w) |
        SetClipboardText(w, _) |
        SetCursor(w, _) |
        NewFavicon(w, _) |
        GetWindowRect(w, _) |
        GetScreenMetrics(w, _) |
        NotifyFullscreenStateChanged(w, _) |
        NotifyLoadStatusChanged(w, _) |
        GetSelectedBluetoothDevice(w, ..) |
        PromptPermission(w, ..) |
        RequestWakeLockPermission(w, ..) |
        InputEventsHandled(w, _) |
        AccessibilityTreeUpdate(w, ..) => o.has(*w),
        ShowNotification(w, _) | ShowConsoleApiMessage(w, ..) => w.is_none_or(|w| o.has(w)),
        ShowEmbedderControl(id, ..) | HideEmbedderControl(id) => o.has(id.webview_id),
        OnDevtoolsStarted(..) | RequestDevtoolsConnection(_) => false,
        // 組み込んでいない機能(ゲームパッドなど)の通知は、通さない。
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

/// ホストの「サイトの鍵」:ドメイン名は登録ドメイン(公開サフィックスの表で求める〈eTLD+1〉)、IP アドレスと `.localhost` は、そのまま。
/// 同じサイト(`a.example.com` と `example.com`)は同じ鍵になり、`com` や別のサイトは違う鍵になる。
fn site_key_with(host: &str, registrable: impl Fn(&str) -> String) -> String {
    let host = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    let ip_like = host.contains(':') || host.chars().all(|c| c.is_ascii_digit() || c == '.');
    if ip_like || host == "localhost" || host.ends_with(".localhost") {
        host
    } else {
        registrable(&host)
    }
}

fn site_key(host: &str) -> String {
    site_key_with(host, |h| net_traits::pub_domains::reg_suffix(h).to_ascii_lowercase())
}

/// ページの URL のホストを、受け持つサイトに足す(ホストの無い URL〈`about:blank`・`data:` など〉は何もしない)。
pub(crate) fn add_site(sites: &Sites, url: &ServoUrl) {
    if let Some(host) = url.host_str() {
        sites.lock().unwrap().insert(site_key(host));
    }
}

fn host_allowed(sites: &HashSet<String>, host: &str) -> bool {
    sites.contains(&site_key(host))
}

fn url_allowed(sites: &Sites, url: &ServoUrl) -> bool {
    match url.host_str() {
        Some(host) => host_allowed(&sites.lock().unwrap(), host),
        // ホストの無い URL(`data:`・`about:` など)の Cookie は、無い。
        None => true,
    }
}

/// ホスト名が、その子の受け持つサイトか(ストレージの検問からも使う)。
pub(crate) fn host_key_allowed(sites: &Sites, host: &str) -> bool {
    host_allowed(&sites.lock().unwrap(), host)
}

fn origin_allowed(sites: &Sites, origin: &Origin) -> bool {
    match origin {
        Origin::Origin(ImmutableOrigin::Tuple(_, host, _)) => {
            host_allowed(&sites.lock().unwrap(), &host.to_string())
        },
        // 不透明な origin(サンドボックスの iframe・`data:`)と、クライアントの origin は、検査できない/しなくてよい。
        _ => true,
    }
}

/// 子に渡してよい要求か。`false` は捨てる。
fn allowed(sites: &Sites, msg: &CoreResourceMsg) -> bool {
    use CoreResourceMsg::*;
    match msg {
        Fetch(builder, _) | FetchRedirect(builder, ..) => {
            // ページの移動(ドキュメント・フレーム)は、送り主(origin)が移動元(別のサイト)になる。移動の取得は、
            // その子が受け持つサイトのページを読み込むもの(取得する URL か、リダイレクトの前の URL が、受け持つサイト)だけ通す。
            let navigation = matches!(
                builder.destination,
                Destination::Document | Destination::IFrame | Destination::Frame
            );
            if navigation {
                return url_allowed(sites, &builder.url) ||
                    builder.url_list.iter().any(|u| url_allowed(sites, u));
            }
            origin_allowed(sites, &builder.origin) &&
                builder
                    .client
                    .as_ref()
                    .is_none_or(|client| origin_allowed(sites, &client.origin))
        },
        SetCookieForUrl(url, ..) |
        SetCookiesForUrl(url, ..) |
        SetCookieForUrlAsync(_, url, ..) |
        GetCookieStringForUrl(url, ..) |
        GetCookiesForUrl(url, ..) |
        GetCookieDataForUrlAsync(_, url, ..) |
        GetAllCookieDataForUrlAsync(_, url, ..) |
        DeleteCookie(url, _) |
        DeleteCookieAsync(_, url, _) |
        NewCookieListener(_, _, url) => url_allowed(sites, url),
        DeleteCookies(Some(url), _) => url_allowed(sites, url),
        // 全部の Cookie の削除・一覧・埋め込み側の操作・キャッシュの操作は、子がする仕事ではない。
        DeleteCookies(None, _) |
        DeleteCookiesForSites(..) |
        DeleteSessionCookies(_) |
        ListCookies(_) |
        EmbedderGetCookiesForUrl(..) |
        EmbedderSetCookieForUrl(..) |
        EmbedderClearCookies(_) |
        EmbedderClearSessionCookies(_) |
        GetCacheEntries(_) |
        ClearCache(_) => false,
        _ => true,
    }
}

/// 子に渡す送り口(検問つきの中継)を作る。要求は、`check` を通ったものだけ、`target` へ渡る(通らないものは捨てる)。
pub(crate) fn relay<T>(
    name: &'static str,
    target: generic_channel::GenericSender<T>,
    check: impl Fn(&T) -> bool + Send + 'static,
) -> generic_channel::GenericSender<T>
where
    T: for<'de> serde::Deserialize<'de> + serde::Serialize + std::fmt::Debug + Send + 'static,
{
    let (sender, receiver) = generic_channel::channel::<T>().expect("guard channel");
    let trace = std::env::var("KYOUSEI_NET_GUARD_TRACE").as_deref() == Ok("1");
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            loop {
                let msg = match receiver.recv() {
                    Ok(msg) => msg,
                    Err(servo_base::generic_channel::ReceiveError::Disconnected) => break,
                    Err(e) => {
                        warn!("{name}: 受信に失敗(読み捨てて続ける): {e:?}");
                        continue;
                    },
                };
                if trace {
                    let d = format!("{msg:?}");
                    warn!("{name} trace: {}", d.chars().take(160).collect::<String>());
                }
                if check(&msg) {
                    if target.send(msg).is_err() {
                        break;
                    }
                } else {
                    warn!("{name}: 受け持たないサイトへの要求を捨てた({:?})", std::mem::discriminant(&msg));
                }
            }
        })
        .expect("guard thread");
    sender
}

/// 子に渡す `CoreResourceThread`(検問つきの中継)を作る。
pub(crate) fn guarded(core: CoreResourceThread, sites: Sites) -> CoreResourceThread {
    relay("net guard", core, move |msg| allowed(&sites, msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 試験用の簡易な公開サフィックス(最後の2つのラベル)。
    fn fake_registrable(h: &str) -> String {
        let labels: Vec<&str> = h.split('.').collect();
        labels[labels.len().saturating_sub(2)..].join(".")
    }
    fn key(h: &str) -> String {
        site_key_with(h, fake_registrable)
    }

    #[test]
    fn subdomains_share_a_site_but_other_sites_and_suffixes_do_not() {
        assert_eq!(key("www.example.com"), key("example.com"));
        assert_eq!(key("A.WWW.Example.com"), key("example.com"));
        assert_ne!(key("bank.com"), key("example.com"));
        assert_ne!(key("evilexample.com"), key("example.com"));
        assert_ne!(key("com"), key("example.com"));
    }

    #[test]
    fn ip_addresses_and_localhost_match_exactly() {
        assert_eq!(key("192.168.1.20"), "192.168.1.20");
        assert_ne!(key("192.168.1.2"), key("192.168.1.20"));
        assert_eq!(key("[::1]"), key("::1"));
        assert_eq!(key("app.localhost"), "app.localhost");
        assert_ne!(key("other.localhost"), key("app.localhost"));
    }
}
