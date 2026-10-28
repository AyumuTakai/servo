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
use paint_api::PaintMessage;
use servo_base::id::{PainterId, PipelineId, TEST_WEBVIEW_ID, WebViewId};
use servo_constellation_traits::ScriptToConstellationMessage;
use net_traits::{CoreResourceMsg, CoreResourceThread};
use servo_base::generic_channel;
use servo_url::{ImmutableOrigin, ServoUrl};

/// その子が受け持つサイトの鍵の集合(`site_key`)。
pub(crate) type Sites = Arc<Mutex<HashSet<String>>>;

/// (Kyousei) その子(コンテンツプロセス)が持つパイプライン(とその面)。子の申告する面(`WebViewId`)・パイプライン(`PipelineId`)が、
/// 本当にその子のものかを、親の側で検査する。constellation が、パイプラインの増減で更新する。
#[derive(Default)]
pub(crate) struct OwnedViews {
    /// パイプライン → (面, 面の最上位のパイプラインか)。
    pipelines: std::collections::HashMap<PipelineId, (WebViewId, bool)>,
    /// 最近なくなったパイプライン(終了の知らせが、constellation の後始末より遅れて届くため)。
    retired: std::collections::VecDeque<(PipelineId, WebViewId)>,
}

const RETIRED_MAX: usize = 512;

pub(crate) type Owned = Arc<Mutex<OwnedViews>>;

impl OwnedViews {
    pub(crate) fn add(&mut self, webview: WebViewId, pipeline: PipelineId, top_level: bool) {
        self.pipelines.insert(pipeline, (webview, top_level));
    }

    pub(crate) fn remove(&mut self, pipeline: PipelineId) {
        if let Some((webview, _)) = self.pipelines.remove(&pipeline) {
            if self.retired.len() >= RETIRED_MAX {
                self.retired.pop_front();
            }
            self.retired.push_back((pipeline, webview));
        }
    }

    /// `owns` に加えて、最近なくなったパイプラインも。
    pub(crate) fn owns_recent(&self, webview: WebViewId, pipeline: PipelineId) -> bool {
        self.owns(webview, pipeline) || self.retired.contains(&(pipeline, webview))
    }

    /// その描画の単位(`PainterId`。面ごと)の面を、この子が持っているか。
    pub(crate) fn has_painter(&self, painter: PainterId) -> bool {
        self.pipelines
            .values()
            .any(|(w, _)| PainterId::from(*w) == painter)
    }

    pub(crate) fn has(&self, webview: WebViewId) -> bool {
        self.pipelines.values().any(|(w, _)| *w == webview)
    }

    pub(crate) fn has_top_level(&self, webview: WebViewId) -> bool {
        self.pipelines.values().any(|(w, top)| *w == webview && *top)
    }

    /// そのパイプラインは、この子のもので、面が合っているか。
    pub(crate) fn owns(&self, webview: WebViewId, pipeline: PipelineId) -> bool {
        self.pipelines
            .get(&pipeline)
            .is_some_and(|(w, _)| *w == webview)
    }
}

/// 子から constellation への要求を通してよいか。送り主として申告された `(面, パイプライン)` が、その子のものでなければ捨てる。
/// iframe の新しいパイプラインは、子が作って名乗り始めるので、親パイプラインが子のものなら、ここで受け持ちに足す
/// (constellation がメッセージを処理するより先に、新しいパイプラインの要求が届くため)。
pub(crate) fn constellation_msg_allowed(
    owned: &Owned,
    msg: &(WebViewId, PipelineId, ScriptToConstellationMessage),
) -> bool {
    let (webview, pipeline, content) = msg;
    // ログは、面・パイプラインに意味のない値(`TEST_*`)で送られる。
    if matches!(content, ScriptToConstellationMessage::LogEntry(..)) {
        return true;
    }
    let mut o = owned.lock().unwrap();
    // ワーカーのグローバルは、面を持たないとき `TEST_WEBVIEW_ID` で名乗る。そのときは、パイプラインだけ見る。
    let owned_by_sender = if *webview == TEST_WEBVIEW_ID {
        o.pipelines.contains_key(pipeline)
    } else {
        o.owns(*webview, *pipeline)
    };
    if !owned_by_sender {
        return false;
    }
    if let ScriptToConstellationMessage::ScriptNewIFrame(load) = content {
        let info = &load.info;
        if info.webview_id != *webview || !o.owns(info.webview_id, info.parent_pipeline_id) {
            return false;
        }
        o.add(info.webview_id, info.new_pipeline_id, false);
    }
    true
}

/// 子から描画(`Paint`)への要求を通してよいか。面を持つものは、その子の面だけ。フォントや画像の更新は、面ごとの描画の単位
/// (`PainterId`)が、その子の面のものだけ。親が自分で送るもの(フレーム木・新しい絵の知らせ)は、子からは通さない。
/// システムのフォントは、親(WebRender)がパスを開くので、フォントらしいファイルで、ホーム・一時の場所などの外のものだけ。
pub(crate) fn paint_msg_allowed(owned: &Owned, msg: &PaintMessage) -> bool {
    use PaintMessage::*;
    let o = owned.lock().unwrap();
    match msg {
        ChangeRunningAnimationsState(w, p, _) |
        SetThrottled(w, p, _) |
        PipelineExited(w, p, _) |
        GenerateImageKeysForPipeline(w, p) |
        DelayNewFrameForCanvas(w, p, ..) |
        UpdateEpoch {
            webview_id: w,
            pipeline_id: p,
            ..
        } => o.owns_recent(*w, *p),
        SendInitialTransaction(w, _) |
        ScrollNodeByDelta(w, ..) |
        ScrollViewportByDelta(w, _) |
        SendDisplayList { webview_id: w, .. } |
        GenerateImageKey(w, _) |
        Viewport(w, _) |
        ScreenshotReadinessReponse(w, _) => o.has(*w),
        UpdateImages(painter, _) |
        GenerateFontKeys(.., painter) |
        AddFont(painter, ..) |
        AddFontInstance(painter, ..) |
        RemoveFonts(painter, ..) => o.has_painter(*painter),
        GenerateFrame(painters) => painters.iter().all(|p| o.has_painter(*p)),
        AddSystemFont(painter, _, handle) => {
            o.has_painter(*painter) && system_font_path_allowed(std::path::Path::new(&handle.path))
        },
        CollectMemoryReport(_) => true,
        SetFrameTreeForWebView(..) | NewWebRenderFrameReady(..) => false,
    }
}

/// 親(WebRender)が開いてよいシステムのフォントのパスか:絶対パスで、`..` を含まず、フォントの拡張子で、
/// 利用者のデータ・一時・システムの情報の場所(`/home`・`/root`・`/tmp`・`/var/tmp`・`/proc`・`/sys`・`/dev`・`/run`)の外。
fn system_font_path_allowed(path: &std::path::Path) -> bool {
    use std::path::Component;
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    const DENIED: &[&str] = &["home", "root", "tmp", "var/tmp", "proc", "sys", "dev", "run"];
    let rel = path.strip_prefix("/").unwrap_or(path);
    if DENIED.iter().any(|d| rel.starts_with(d)) {
        return false;
    }
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2" | "pfb" | "pfa" | "pcf" | "gz"
        )
    })
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

    fn ids() -> (WebViewId, PipelineId) {
        use servo_base::id::{PainterId, PipelineNamespace, PipelineNamespaceId};
        thread_local!(static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
        if !INSTALLED.with(|i| i.replace(true)) {
            PipelineNamespace::install(PipelineNamespaceId(7));
        }
        (WebViewId::new(PainterId::next()), PipelineId::new())
    }

    #[test]
    fn a_child_only_owns_the_pipelines_and_views_it_was_given() {
        let (w1, p1) = ids();
        let (w2, p2) = ids();
        let mut o = OwnedViews::default();
        o.add(w1, p1, true);
        assert!(o.owns(w1, p1) && o.has(w1) && o.has_top_level(w1));
        // 別の子の面・パイプライン、食い違う組み合わせは、受け持たない。
        assert!(!o.owns(w2, p2) && !o.owns(w1, p2) && !o.owns(w2, p1) && !o.has(w2));
        // iframe のパイプラインは、最上位の印が付かない。面を持っていても、最上位のページは持たない。
        let (w3, p3) = ids();
        o.add(w3, p3, false);
        assert!(o.has(w3) && !o.has_top_level(w3));
        o.remove(p1);
        assert!(!o.has(w1) && !o.owns(w1, p1));
    }

    #[test]
    fn system_font_paths_must_look_like_fonts_outside_user_and_system_info_dirs() {
        use std::path::Path;
        assert!(system_font_path_allowed(Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")));
        assert!(system_font_path_allowed(Path::new("/nix/store/x/Noto.OTC")));
        assert!(!system_font_path_allowed(Path::new("/home/u/.local/share/kyousei/store.db")));
        assert!(!system_font_path_allowed(Path::new("/home/u/font.ttf")));
        assert!(!system_font_path_allowed(Path::new("/proc/self/mem")));
        assert!(!system_font_path_allowed(Path::new("/usr/share/fonts/../../etc/shadow.ttf")));
        assert!(!system_font_path_allowed(Path::new("relative/font.ttf")));
        assert!(!system_font_path_allowed(Path::new("/etc/passwd")));
    }

    #[test]
    fn a_retired_pipeline_is_still_accepted_for_its_exit_notice() {
        let (w1, p1) = ids();
        let mut o = OwnedViews::default();
        o.add(w1, p1, true);
        o.remove(p1);
        assert!(!o.owns(w1, p1) && o.owns_recent(w1, p1));
        let (w2, _) = ids();
        assert!(!o.owns_recent(w2, p1));
    }

    #[test]
    fn embedder_messages_for_other_views_are_dropped() {
        let (w1, p1) = ids();
        let (w2, _) = ids();
        let owned = Owned::default();
        owned.lock().unwrap().add(w1, p1, true);
        assert!(embedder_msg_allowed(&owned, &EmbedderMsg::ClearClipboard(w1)));
        assert!(!embedder_msg_allowed(&owned, &EmbedderMsg::ClearClipboard(w2)));
        assert!(!embedder_msg_allowed(
            &owned,
            &EmbedderMsg::OnDevtoolsStarted(Err(()), String::new())
        ));
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
