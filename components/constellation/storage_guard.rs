/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! (Kyousei) コンテンツプロセスから、親のストレージのスレッド(Web ストレージ・IndexedDB・クライアントストレージ・
//! キャッシュストレージ)へ向かう要求の検問。`net_guard.rs` と同じ考えで、要求の origin が、その子が受け持つサイトに
//! 当てはまらないものを捨てる。**変種は、すべて列挙している**(Servo を上げて変種が増えたら、コンパイルが通らず、気付ける)。

use storage_traits::StorageThreads;
use storage_traits::cache_storage::CacheStorageThreadMessage;
use storage_traits::client_storage::ClientStorageThreadMessage;
use storage_traits::indexeddb::{IndexedDBThreadMsg, SyncOperation};
use storage_traits::webstorage_thread::WebStorageThreadMsg;

use crate::net_guard::{Sites, relay};

/// 不透明な origin(サンドボックスの iframe など)は、持続するストレージを持たないので、通す。
fn origin_ok(sites: &Sites, origin: &servo_url::ImmutableOrigin) -> bool {
    match origin {
        servo_url::ImmutableOrigin::Tuple(_, host, _) => {
            crate::net_guard::host_key_allowed(sites, &host.to_string())
        },
        servo_url::ImmutableOrigin::Opaque(_) => true,
    }
}

fn web_storage(sites: &Sites, msg: &WebStorageThreadMsg) -> bool {
    use WebStorageThreadMsg::*;
    match msg {
        Length(_, _, _, o) | Keys(_, _, _, o) | Clear(_, _, _, o) => origin_ok(sites, o),
        Key(_, _, _, o, _) | GetItem(_, _, _, o, _) | RemoveItem(_, _, _, o, _) => {
            origin_ok(sites, o)
        },
        SetItem(_, _, _, o, _, _) => origin_ok(sites, o),
        // `window.open` で、開いた側の sessionStorage を写す(同じプロセスの中の動き)。
        Clone { .. } => true,
        // 全部の origin の一覧・サイトごとの削除・終了・メモリの報告は、子がする仕事ではない。
        ListOrigins(..) | ClearDataForSites(..) | Exit(_) | CollectMemoryReport(_) => false,
    }
}

fn client_storage(sites: &Sites, msg: &ClientStorageThreadMessage) -> bool {
    use ClientStorageThreadMessage::*;
    match msg {
        ObtainBottleMap { origin, .. } |
        Persisted { origin, .. } |
        Persist { origin, .. } |
        Estimate { origin, .. } => origin_ok(sites, origin),
        // データベースの作成・削除(番号だけで指す)と終了は、親の中のスレッドが頼む。子は頼まない。
        CreateDatabase { .. } | DeleteDatabase { .. } | Exit(_) => false,
    }
}

fn indexed_db(sites: &Sites, msg: &IndexedDBThreadMsg) -> bool {
    use IndexedDBThreadMsg::*;
    match msg {
        Sync(op) => sync_operation(sites, op),
        Async(o, ..) => origin_ok(sites, o),
        AsyncSchemaOperation { origin, .. } | EngineTxnBatchComplete { origin, .. } => {
            origin_ok(sites, origin)
        },
        CollectMemoryReport(_) => false,
    }
}

fn sync_operation(sites: &Sites, op: &SyncOperation) -> bool {
    use SyncOperation::*;
    match op {
        GetDatabases(_, o) |
        UpgradeVersion(_, o, ..) |
        GetObjectStore(_, o, ..) |
        Commit(_, o, ..) |
        Abort(_, o, ..) |
        CloseDatabase(o, ..) |
        OpenDatabase(_, o, ..) |
        DeleteDatabase(_, o, ..) |
        Version(_, o, ..) => origin_ok(sites, o),
        UpgradeTransactionFinished { origin, .. } |
        RequestHandled { origin, .. } |
        CreateTransaction { origin, .. } |
        TxnMaybeCommit { origin, .. } |
        TransactionFinished { origin, .. } |
        AbortPendingUpgrades { origin, .. } |
        NotifyEndOfVersionChange { origin, .. } => origin_ok(sites, origin),
        Exit(_) => false,
    }
}

fn cache_storage(sites: &Sites, msg: &CacheStorageThreadMessage) -> bool {
    use CacheStorageThreadMessage::*;
    match msg {
        HasCache { origin, .. } |
        OpenCache { origin, .. } |
        Keys { origin, .. } |
        DeleteCache { origin, .. } => origin_ok(sites, origin),
        Exit(_) => false,
    }
}

/// 子に渡す `StorageThreads`(4つの口すべてに、検問つきの中継)を作る。
pub(crate) fn guarded(threads: &StorageThreads, sites: Sites) -> StorageThreads {
    let (s1, s2, s3, s4) = (sites.clone(), sites.clone(), sites.clone(), sites);
    threads.map_senders(
        |t| relay("client-storage guard", t, move |m| client_storage(&s1, m)),
        |t| relay("indexeddb guard", t, move |m| indexed_db(&s2, m)),
        |t| relay("web-storage guard", t, move |m| web_storage(&s3, m)),
        |t| relay("cache-storage guard", t, move |m| cache_storage(&s4, m)),
    )
}
