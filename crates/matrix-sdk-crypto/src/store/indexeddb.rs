// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(dead_code)]

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    convert::{TryFrom, TryInto},
};
use wasm_bindgen::JsValue;

use dashmap::DashSet;
use matrix_sdk_common::{async_trait, locks::Mutex, uuid};
use olm_rs::{account::IdentityKeys, PicklingMode};
use ruma::{
    events::{room_key_request::RequestedKeyInfo, secret::request::SecretName},
    DeviceId, DeviceIdBox, RoomId, UserId,
};
use tracing::trace;
use uuid::Uuid;

use super::{
    caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, InboundGroupSession,
    PickleKey, EncryptedPickleKey, ReadOnlyAccount, Result, RoomKeyCounts, Session,
};
use crate::{
    gossiping::{GossipRequest, SecretInfo},
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    olm::{OutboundGroupSession, PickledInboundGroupSession, PrivateCrossSigningIdentity},
};
use indexed_db_futures::{prelude::*, web_sys::IdbKeyRange};

/// This needs to be 32 bytes long since AES-GCM requires it, otherwise we will
/// panic once we try to pickle a Signing object.
const DEFAULT_PICKLE: &str = "DEFAULT_PICKLE_PASSPHRASE_123456";

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

#[allow(non_snake_case)]
mod KEYS {

    // STORES
    pub const CORE: &'static str = "core";

    pub const SESSION: &'static str = "session";
    pub const INBOUND_GROUP_SESSIONS: &'static str = "inbound_group_sessions";

    pub const OUTBOUND_GROUP_SESSIONS: &'static str = "outbound_group_sessions";

    pub const TRACKED_USERS: &'static str = "tracked_users";
    pub const OLM_HASHES: &'static str = "olm_hashes";

    pub const DEVICES: &'static str = "devices";
    pub const IDENTITIES: &'static str = "identities";

    pub const OUTGOING_SECRET_REQUESTS: &'static str = "outgoing_secret_requests";
    pub const UNSENT_SECRET_REQUESTS: &'static str = "unsent_secret_requests";
    pub const SECRET_REQUESTS_BY_INFO: &'static str = "secret_requests_by_info";

    // KEYS
   pub const PICKLE_KEY: &'static str = "pickle_key";
   pub const ACCOUNT: &'static str = "account";
   pub const PRIVATE_IDENTITY: &'static str = "private_identity";
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
pub struct IndexeddbStore {
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    name: String,
    pub(crate) inner: IdbDatabase,
    pickle_key: Arc<PickleKey>,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<UserId>>,
    users_for_key_query_cache: Arc<DashSet<UserId>>,

}

impl std::fmt::Debug for IndexeddbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbStore").field("name", &self.name).finish()
    }
}

fn make_range(key: String) -> Result<IdbKeyRange, CryptoStoreError> {
    IdbKeyRange::bound(
        &JsValue::from_str(&format!("{}:", key)),
        &JsValue::from_str(&format!("{};", key)),
    )
    .map_err(|e| {
       CryptoStoreError::IndexedDatabase {
            code: 0,
            name: "IdbKeyRangeMakeError".to_owned(),
            message: e.as_string().unwrap_or(format!("Creating key range for {:} failed", key)),
       }
    })
}

impl IndexeddbStore {

    async fn open_helper(prefix: String, pickle_key: Option<PickleKey>) -> Result<Self> {
        let name = format!("{:0}::matrix-sdk-crypto", prefix);

        let pickle_key = pickle_key.unwrap_or_else(||
            PickleKey::try_from(DEFAULT_PICKLE.as_bytes().to_vec())
                .expect("Default Pickle always works. qed")
        );

        // Open my_db v1
        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store(KEYS::CORE)?;
                db.create_object_store(KEYS::SESSION)?;

                db.create_object_store(KEYS::INBOUND_GROUP_SESSIONS)?;
                db.create_object_store(KEYS::OUTBOUND_GROUP_SESSIONS)?;
                db.create_object_store(KEYS::TRACKED_USERS)?;
                db.create_object_store(KEYS::OLM_HASHES)?;
                db.create_object_store(KEYS::DEVICES)?;

                db.create_object_store(KEYS::IDENTITIES)?;
                db.create_object_store(KEYS::OUTGOING_SECRET_REQUESTS)?;
                db.create_object_store(KEYS::UNSENT_SECRET_REQUESTS)?;
                db.create_object_store(KEYS::SECRET_REQUESTS_BY_INFO)?;
            }
            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;
        let session_cache = SessionStore::new();

        Ok(Self {
            name,
            session_cache,
            pickle_key: pickle_key.into(),
            inner: db,
            account_info: RwLock::new(None).into(),
            tracked_users_cache: DashSet::new().into(),
            users_for_key_query_cache: DashSet::new().into(),
        })
    }

    pub async fn open() -> Result<Self> {
        IndexeddbStore::open_helper("crypto".to_owned(), None).await
    }

    pub async fn open_with_passphrase(prefix: String, passphrase: &str) -> Result<Self> {
        let name = format!("{:0}::matrix-sdk-crypto-meta", prefix);

        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store("matrix-sdk-crypto")?;
            }
            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;

        let tx: IdbTransaction =
            db.transaction_on_one_with_mode("matrix-sdk-crypto", IdbTransactionMode::Readwrite)?;
        let ob = tx.object_store("matrix-sdk-crypto")?;

        let store_key: Option<EncryptedPickleKey> = ob
            .get(&JsValue::from_str(KEYS::PICKLE_KEY))?
            .await?
            .map(|k| k.into_serde())
            .transpose()?;

        let pickle_key = match store_key {
            Some(key) => PickleKey::from_encrypted(passphrase, key)
                            .map_err(|_| CryptoStoreError::UnpicklingError)?,
            None => {
                let key = PickleKey::new();
                let encrypted = key.encrypt(passphrase);
                ob.put_key_val(&JsValue::from_str(KEYS::PICKLE_KEY), &JsValue::from_serde(&encrypted)?)?;
                tx.await.into_result()?;
                key
            }
        };

        IndexeddbStore::open_helper(prefix, Some(pickle_key)).await
    }

    pub async fn open_with_name(name: String) -> Result<Self> {
        IndexeddbStore::open_helper(name, None).await
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }


    fn get_pickle_mode(&self) -> PicklingMode {
        self.pickle_key.pickle_mode()
    }

    fn get_pickle_key(&self) -> &[u8] {
        self.pickle_key.key()
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let mut stores: Vec<&'static str> = [
            (changes.account.is_some() || changes.private_identity.is_some(), KEYS::CORE),
            (!changes.sessions.is_empty(), KEYS::SESSION),
            (!changes.devices.new.is_empty() || !changes.devices.changed.is_empty() || !changes.devices.deleted.is_empty(),
                KEYS::DEVICES),
            (!changes.identities.new.is_empty() || !changes.identities.changed.is_empty(),
                KEYS::IDENTITIES),
            (!changes.inbound_group_sessions.is_empty(),  KEYS::INBOUND_GROUP_SESSIONS),
            (!changes.outbound_group_sessions.is_empty(), KEYS::OUTBOUND_GROUP_SESSIONS),
            (!changes.message_hashes.is_empty(), KEYS::OLM_HASHES),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.key_requests.is_empty() {
            stores.extend([
                KEYS::SECRET_REQUESTS_BY_INFO,
                KEYS::UNSENT_SECRET_REQUESTS,
                KEYS::OUTGOING_SECRET_REQUESTS,
            ])
        }

        if stores.len() == 0 {
            // nothing to do, quit early
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        let account_pickle = if let Some(a) = changes.account {
            Some(a.pickle(self.get_pickle_mode()).await)
        } else {
            None
        };

        let private_identity_pickle = if let Some(i) = changes.private_identity {
            Some(i.pickle(self.get_pickle_key()).await?)
        } else {
            None
        };

        if let Some(a) = &account_pickle {
            tx.object_store(KEYS::CORE)?
                .put_key_val(&JsValue::from_str(KEYS::ACCOUNT), &JsValue::from_serde(&a)?)?;
        }

        if let Some(i) = &private_identity_pickle {
            tx.object_store(KEYS::CORE)?
                .put_key_val(&JsValue::from_str(KEYS::PRIVATE_IDENTITY), &JsValue::from_serde(i)?)?;
        }


        if !changes.sessions.is_empty() {
            let sessions = tx.object_store(KEYS::SESSION)?;

            for session in &changes.sessions {
                let sender_key = session.sender_key();
                let session_id = session.session_id();

                let pickle = session.pickle(self.get_pickle_mode()).await;
                let key = format!("{}:{}", sender_key, session_id);

                sessions.put_key_val(&JsValue::from_str(&key), &JsValue::from_serde(&pickle)?)?;
            }
        }

        if !changes.inbound_group_sessions.is_empty() {
            let sessions = tx.object_store(KEYS::INBOUND_GROUP_SESSIONS)?;

            for session in changes.inbound_group_sessions {
                let room_id = session.room_id();
                let sender_key = session.sender_key();
                let session_id = session.session_id();
                let key = format!("{}:{}:{}", room_id, sender_key, session_id);
                let pickle = session.pickle(self.get_pickle_mode()).await;

                sessions.put_key_val(&JsValue::from_str(&key), &JsValue::from_serde(&pickle)?)?;
            }
        }

        if !changes.outbound_group_sessions.is_empty() {
            let sessions = tx.object_store(KEYS::OUTBOUND_GROUP_SESSIONS)?;

            for session in changes.outbound_group_sessions {
                let room_id = session.room_id();
                let pickle = session.pickle(self.get_pickle_mode()).await;
                sessions.put_key_val(&JsValue::from_str(room_id.as_str()), &JsValue::from_serde(&pickle)?)?;
            }
        }

        let device_changes = changes.devices;
        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;

        if !device_changes.new.is_empty() || !device_changes.changed.is_empty() {
            let device_store = tx.object_store(KEYS::DEVICES)?;
            for device in device_changes.new.iter().chain(&device_changes.changed) {
                let key = format!("{}:{}", device.user_id().as_str(), device.device_id().as_str());
                let device = JsValue::from_serde(&device)?;

                device_store.put_key_val(&JsValue::from_str(&key), &device)?;
            }
        }

        if !device_changes.deleted.is_empty() {
            let device_store = tx.object_store(KEYS::DEVICES)?;

            for device in &device_changes.deleted {
                let key = format!("{}:{}", device.user_id().as_str(), device.device_id().as_str());
                device_store.delete(&JsValue::from_str(&key))?;
            }
        }

        if !identity_changes.changed.is_empty() || !identity_changes.new.is_empty() {
            let identities = tx.object_store(KEYS::IDENTITIES)?;
            for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                identities.put_key_val(
                    &JsValue::from_str(identity.user_id().as_str()),
                    &JsValue::from_serde(&identity)?,
                )?;
            }
        }

        if !olm_hashes.is_empty() {
            let hashes = tx.object_store(KEYS::OLM_HASHES)?;
            for hash in &olm_hashes {
                hashes.put_key_val(
                    &JsValue::from_str(&format!("{}:{}", hash.sender_key, hash.hash)),
                    &JsValue::TRUE
                )?;
            }
        }

        if !key_requests.is_empty() {
            let secret_requests_by_info = tx.object_store(KEYS::SECRET_REQUESTS_BY_INFO)?;
            let unsent_secret_requests = tx.object_store(KEYS::UNSENT_SECRET_REQUESTS)?;
            let outgoing_secret_requests = tx.object_store(KEYS::OUTGOING_SECRET_REQUESTS)?;
            for key_request in &key_requests {
                let key_request_id = JsValue::from_str(&key_request.request_id.to_string());
                secret_requests_by_info.put_key_val(
                    &JsValue::from_str(&key_request.info.as_key()),
                    &key_request_id,
                )?;

                if key_request.sent_out {
                    unsent_secret_requests.delete(&key_request_id)?;
                    outgoing_secret_requests.put_key_val(
                        &key_request_id,
                        &JsValue::from_serde(&key_request)?,
                    )?;
                } else {
                    outgoing_secret_requests.delete(&key_request_id)?;
                    unsent_secret_requests.put_key_val(
                        &key_request_id,
                        &JsValue::from_serde(&key_request)?,
                    )?;
                }
            }
        }

        tx.await.into_result()?;

        // all good, let's update our caches:indexeddb
        for session in changes.sessions {
            self.session_cache.add(session).await;
        }

        Ok(())
    }


    async fn load_tracked_users(&self) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::TRACKED_USERS, IdbTransactionMode::Readonly)?;
        let os = tx.object_store(KEYS::TRACKED_USERS)?;
        let user_ids = os
            .get_all_keys()?
            .await?;
        for user_id in user_ids.iter() {
            let dirty : bool = match os.get(&user_id)?.await?.map(|v| v.into_serde()) {
                Some(Ok(false)) => false,
                _ => true
            };
            let user = match user_id.as_string().map(|u| UserId::try_from(u)) {
                Some(Ok(user)) => user,
                _ => continue
            };
            self.tracked_users_cache.insert(user.clone());

            if dirty {
                self.users_for_key_query_cache.insert(user);
            }
        }

        Ok(())
    }

    async fn load_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        todo!()
        // let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        // self.outbound_group_sessions
        //     .get(room_id.encode())?
        //     .map(|p| serde_json::from_slice(&p).map_err(CryptoStoreError::Serialization))
        //     .transpose()?
        //     .map(|p| {
        //         OutboundGroupSession::from_pickle(
        //             account_info.device_id,
        //             account_info.identity_keys,
        //             p,
        //             self.get_pickle_mode(),
        //         )
        //         .map_err(CryptoStoreError::OlmGroupSession)
        //     })
        //     .transpose()
    }
    async fn get_outgoing_key_request_helper(&self, key: &str) -> Result<Option<GossipRequest>> {
        let jskey = JsValue::from_str(key);
        let dbs = [KEYS::OUTGOING_SECRET_REQUESTS, KEYS::UNSENT_SECRET_REQUESTS];
        let tx = self
             .inner
             .transaction_on_multi_with_mode(&dbs, IdbTransactionMode::Readonly)?;

        let request = tx.object_store(KEYS::OUTGOING_SECRET_REQUESTS)?
             .get(&jskey)?
             .await?
             .map(|i| i.into_serde())
             .transpose()?;

        Ok(match request {
            None =>  tx.object_store(KEYS::UNSENT_SECRET_REQUESTS)?
                .get(&jskey)?
                .await?
                .map(|i| i.into_serde())
                .transpose()?,
            Some(request) => Some(request),
        })
    }
}

#[async_trait(?Send)]
impl CryptoStore for IndexeddbStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(KEYS::CORE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::CORE)?
            .get(&JsValue::from_str(KEYS::ACCOUNT))?
            .await?
        {

            self.load_tracked_users().await?;

            let account = ReadOnlyAccount::from_pickle(pickle.into_serde()?, self.get_pickle_mode())?;

            let account_info = AccountInfo {
                user_id: account.user_id.clone(),
                device_id: account.device_id.clone(),
                identity_keys: account.identity_keys.clone(),
            };

            *self.account_info.write().unwrap() = Some(account_info);

            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()> {
        let account_info = AccountInfo {
            user_id: account.user_id.clone(),
            device_id: account.device_id.clone(),
            identity_keys: account.identity_keys.clone(),
        };

        *self.account_info.write().unwrap() = Some(account_info);

        let changes = Changes { account: Some(account), ..Default::default() };

        self.save_changes(changes).await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {

        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(KEYS::CORE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::CORE)?
            .get(&JsValue::from_str(KEYS::PRIVATE_IDENTITY))?
            .await?
        {
            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle.into_serde()?, self.get_pickle_key())
                    .await
                    .map_err(|_| CryptoStoreError::UnpicklingError)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let range = make_range(sender_key.to_owned())?;
            let sessions: Vec<Session> = self
                .inner
                .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
                .object_store(KEYS::SESSION)?
                .get_all_with_key(&range)?
                .await?
                .iter()
                .filter_map(|f| match f.into_serde() {
                    Ok(p) => Session::from_pickle(
                            account_info.user_id.clone(),
                            account_info.device_id.clone(),
                            account_info.identity_keys.clone(),
                            p,
                            self.get_pickle_mode(),
                        ).ok(),
                    _ => None
                })
                .collect::<Vec<Session>>();

            self.session_cache.set_for_sender(sender_key, sessions);
        }

        Ok(self.session_cache.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key = format!("{}:{}:{}", room_id.as_str(), sender_key, session_id);
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(KEYS::INBOUND_GROUP_SESSIONS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::INBOUND_GROUP_SESSIONS)?
            .get(&JsValue::from_str(&key))?
            .await?
        {
            Ok(Some(
                InboundGroupSession::from_pickle(pickle.into_serde()?, self.get_pickle_mode())?
            ))
        } else {
            Ok(None)
        }
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        todo!()
        // let pickles: Result<Vec<PickledInboundGroupSession>> = self
        //     .inbound_group_sessions
        //     .iter()
        //     .map(|p| serde_json::from_slice(&p?.1).map_err(CryptoStoreError::Serialization))
        //     .collect();

        // Ok(pickles?
        //     .into_iter()
        //     .filter_map(|p| InboundGroupSession::from_pickle(p, self.get_pickle_mode()).ok())
        //     .collect())
    }

    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        todo!()
        // self.load_outbound_group_session(room_id).await
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        todo!()
        // let backed_up =
        //     self.get_inbound_group_sessions().await?.into_iter().filter(|s| s.backed_up()).count();

        // Ok(RoomKeyCounts { total: self.inbound_group_sessions.count(), backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        todo!()
        // Ok(self
        //     .get_inbound_group_sessions()
        //     .await?
        //     .into_iter()
        //     .filter(|s| !s.backed_up())
        //     .take(limit)
        //     .collect())
    }

    async fn reset_backup_state(&self) -> Result<()> {
        todo!()
        // for session in self.get_inbound_group_sessions().await? {
        //     session.reset_backup_state()
        // }

        // Ok(())
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users_cache.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query_cache.is_empty()
    }

    fn tracked_users(&self) -> HashSet<UserId> {
        self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect()
    }

    fn users_for_key_query(&self) -> HashSet<UserId> {
        self.users_for_key_query_cache.iter().map(|u| u.clone()).collect()
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        todo!()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users_cache.insert(user.clone());

        if dirty {
            self.users_for_key_query_cache.insert(user.clone());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::TRACKED_USERS, IdbTransactionMode::Readwrite)?;
        let os = tx.object_store(KEYS::TRACKED_USERS)?;

        os.put_key_val(
            &JsValue::from_str(user.as_str()),
            &match dirty { true => JsValue::TRUE, false => JsValue::FALSE }
        )?;

        tx.await.into_result()?;
        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let key = format!("{}:{}", user_id.as_str(), device_id.as_str());
        Ok(self
             .inner
             .transaction_on_one_with_mode(KEYS::DEVICES, IdbTransactionMode::Readonly)?
             .object_store(KEYS::DEVICES)?
             .get(&JsValue::from_str(&key))?
             .await?
             .map(|i| i.into_serde())
             .transpose()?
         )
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        let range = make_range(user_id.as_str().to_string())?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::DEVICES)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|d| {
                let d: ReadOnlyDevice = d.into_serde().ok()?;
                Some((d.device_id().to_owned(), d))
            })
            .collect::<HashMap<_, _>>())
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
       Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::IDENTITIES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::IDENTITIES)?
            .get(&JsValue::from_str(user_id.as_str()))?
            .await?
            .map(|i| i.into_serde())
            .transpose()?
        )
    }

    async fn is_message_known(&self, hash: &crate::olm::OlmMessageHash) -> Result<bool> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::OLM_HASHES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::OLM_HASHES)?
            .get(&JsValue::from_str(&format!("{}:{}", hash.sender_key, hash.hash)))?
            .await?
            .is_some()
        )
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: Uuid,
    ) -> Result<Option<GossipRequest>> {
        self.get_outgoing_key_request_helper(&request_id.to_string()).await
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let id = self
             .inner
             .transaction_on_one_with_mode(KEYS::SECRET_REQUESTS_BY_INFO, IdbTransactionMode::Readonly)?
             .object_store(KEYS::SECRET_REQUESTS_BY_INFO)?
             .get(&JsValue::from_str(&key_info.as_key()))?
             .await?
             .map(|i| i.as_string())
             .flatten();
        if let Some(id) = id {
            self.get_outgoing_key_request_helper(&id).await
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        Ok(self
             .inner
             .transaction_on_one_with_mode(KEYS::UNSENT_SECRET_REQUESTS, IdbTransactionMode::Readonly)?
             .object_store(KEYS::UNSENT_SECRET_REQUESTS)?
             .get_all()?
             .await?
             .iter()
             .filter_map(|i| i.into_serde().ok())
             .collect()
        )
    }

    async fn delete_outgoing_secret_requests(&self, request_id: Uuid) -> Result<()> {
        let jskey = JsValue::from_str(&request_id.to_string());
        let dbs = [KEYS::OUTGOING_SECRET_REQUESTS, KEYS::UNSENT_SECRET_REQUESTS, KEYS::SECRET_REQUESTS_BY_INFO];
        let tx = self
             .inner
             .transaction_on_multi_with_mode(&dbs, IdbTransactionMode::Readwrite)?;

        let request : Option<GossipRequest> = tx.object_store(KEYS::OUTGOING_SECRET_REQUESTS)?
             .get(&jskey)?
             .await?
             .map(|i| i.into_serde())
             .transpose()?;

        let request  = match request {
            None =>  tx.object_store(KEYS::UNSENT_SECRET_REQUESTS)?
                .get(&jskey)?
                .await?
                .map(|i| i.into_serde())
                .transpose()?,
            Some(request) => Some(request),
        };

        if let Some(inner) = request {
            tx.object_store(KEYS::SECRET_REQUESTS_BY_INFO)?
                .delete(&JsValue::from_str(&inner.info.as_key()))?;
        }

        tx.object_store(KEYS::UNSENT_SECRET_REQUESTS)?.delete(&jskey)?;
        tx.object_store(KEYS::OUTGOING_SECRET_REQUESTS)?.delete(&jskey)?;

        tx.await.into_result().map_err(|e| e.into())
    }
}

#[cfg(test)]
mod test {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use wasm_bindgen_test::wasm_bindgen_test;
    use std::collections::BTreeMap;

    use matrix_sdk_common::uuid::Uuid;
    use matrix_sdk_test::async_test;
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use ruma::{
        encryption::SignedKey, events::room_key_request::RequestedKeyInfo, room_id, user_id,
        DeviceId, EventEncryptionAlgorithm, UserId,
    };

    use super::{CryptoStore, GossipRequest, IndexeddbStore};
    use crate::{
        gossiping::SecretInfo,
        identities::{
            device::test::get_device,
            user::test::{get_other_identity, get_own_identity},
        },
        olm::{
            GroupSessionKey, InboundGroupSession, OlmMessageHash, PrivateCrossSigningIdentity,
            ReadOnlyAccount, Session,
        },
        store::{Changes, DeviceChanges, IdentityChanges},
    };

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> Box<DeviceId> {
        "ALICEDEVICE".into()
    }

    fn bob_id() -> UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVICE".into()
    }

    async fn get_store(name: String, passphrase: Option<&str>) -> IndexeddbStore {
        match passphrase {
            Some(pass) => IndexeddbStore::open_with_passphrase(name, pass)
                .await
                .expect("Can't create a passphrase protected store"),
            None => IndexeddbStore::open_with_name(name)
                .await
                .expect("Can't create store without passphrase"),
        }

    }

    async fn get_loaded_store(name: String) -> (ReadOnlyAccount, IndexeddbStore) {
        let store = get_store(name, None).await;
        let account = get_account();
        store.save_account(account.clone()).await.expect("Can't save account");

        (account, store)
    }

    fn get_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&alice_id(), &alice_device_id())
    }

    async fn get_account_and_session() -> (ReadOnlyAccount, Session) {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key =
            bob.one_time_keys().await.curve25519().iter().next().unwrap().1.to_owned();
        let one_time_key = SignedKey::new(one_time_key, BTreeMap::new());
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session =
            alice.create_outbound_session_helper(&sender_key, &one_time_key).await.unwrap();

        (alice, session)
    }

    #[async_test]
    async fn save_account() {
        let store = get_store("save_account".to_owned(),  None).await;
        assert!(store.load_account().await.unwrap().is_none());
        let account = get_account();

        store.save_account(account).await.expect("Can't save account");
    }

    #[async_test]
    async fn load_account() {
        let store = get_store("load_account".to_owned(), None).await;
        let account = get_account();

        store.save_account(account.clone()).await.expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[async_test]
    async fn load_account_with_passphrase() {
        let store = get_store("load_account_with_passphrase".to_owned(), Some("secret_passphrase")).await;
        let account = get_account();

        store.save_account(account.clone()).await.expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[async_test]
    async fn save_and_share_account() {
        let store = get_store("save_and_share_account".to_owned(), None).await;
        let account = get_account();

        store.save_account(account.clone()).await.expect("Can't save account");

        account.mark_as_shared();
        account.update_uploaded_key_count(50);

        store.save_account(account.clone()).await.expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
        assert_eq!(account.uploaded_key_count(), loaded_account.uploaded_key_count());
    }

    #[async_test]
    async fn load_sessions() {
        let store = get_store("load_sessions".to_owned(), None).await;
        let (account, session) = get_account_and_session().await;
        store.save_account(account.clone()).await.expect("Can't save account");

        let changes = Changes { sessions: vec![session.clone()], ..Default::default() };

        store.save_changes(changes).await.unwrap();

        let sessions =
            store.get_sessions(&session.sender_key).await.expect("Can't load sessions").unwrap();
        let loaded_session = sessions.lock().await.get(0).cloned().unwrap();

        assert_eq!(&session, &loaded_session);
    }

    #[async_test]
    async fn add_and_save_session() {
        let store_name = "add_and_save_session".to_owned();
        let store = get_store(store_name.clone(), None).await;
        let (account, session) = get_account_and_session().await;
        let sender_key = session.sender_key.to_owned();
        let session_id = session.session_id().to_owned();

        store.save_account(account.clone()).await.expect("Can't save account");

        let changes = Changes { sessions: vec![session.clone()], ..Default::default() };
        store.save_changes(changes).await.unwrap();

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());

        drop(store);

        let store = IndexeddbStore::open_with_name(store_name)
            .await
            .expect("Can't create store");

        let loaded_account = store.load_account().await.unwrap().unwrap();
        assert_eq!(account, loaded_account);

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());
    }

    #[async_test]
    async fn save_inbound_group_session() {
        let (account, store) = get_loaded_store("save_inbound_group_session".to_owned()).await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
            None,
        )
        .expect("Can't create session");

        let changes = Changes { inbound_group_sessions: vec![session], ..Default::default() };

        store.save_changes(changes).await.expect("Can't save group session");
    }

    #[async_test]
    async fn load_inbound_group_session() {
        let dir = "load_inbound_group_session".to_owned();
        let (account, store) = get_loaded_store(dir.clone()).await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
            None,
        )
        .expect("Can't create session");

        let mut export = session.export().await;

        export.forwarding_curve25519_key_chain = vec!["some_chain".to_owned()];

        let session = InboundGroupSession::from_export(export).unwrap();

        let changes =
            Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

        store.save_changes(changes).await.expect("Can't save group session");

        drop(store);

        let store = IndexeddbStore::open_with_name(dir).await.expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_session = store
            .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session, loaded_session);
        let export = loaded_session.export().await;
        assert!(!export.forwarding_curve25519_key_chain.is_empty())
    }

    #[async_test]
    async fn test_tracked_users() {
        let dir = "test_tracked_users".to_owned();
        let (_account, store) = get_loaded_store(dir.clone()).await;
        let device = get_device();

        assert!(store.update_tracked_user(device.user_id(), false).await.unwrap());
        assert!(!store.update_tracked_user(device.user_id(), false).await.unwrap());

        assert!(store.is_user_tracked(device.user_id()));
        assert!(!store.users_for_key_query().contains(device.user_id()));
        assert!(!store.update_tracked_user(device.user_id(), true).await.unwrap());
        assert!(store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let store = IndexeddbStore::open_with_name(dir.clone()).await.expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(store.is_user_tracked(device.user_id()));
        assert!(store.users_for_key_query().contains(device.user_id()));

        store.update_tracked_user(device.user_id(), false).await.unwrap();
        assert!(!store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let store = IndexeddbStore::open_with_name(dir).await.expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(!store.users_for_key_query().contains(device.user_id()));
    }

    #[async_test]
    async fn device_saving() {
        let dir = "device_saving".to_owned();
        let (_account, store) = get_loaded_store(dir.clone()).await;
        let device = get_device();

        let changes = Changes {
            devices: DeviceChanges { changed: vec![device.clone()], ..Default::default() },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        drop(store);

        let store = IndexeddbStore::open_with_name(dir).await.expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device =
            store.get_device(device.user_id(), device.device_id()).await.unwrap().unwrap();

        assert_eq!(device, loaded_device);

        for algorithm in loaded_device.algorithms() {
            assert!(device.algorithms().contains(algorithm));
        }
        assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
        assert_eq!(device.keys(), loaded_device.keys());

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
        assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.values().next().unwrap(), &device);
    }

    #[async_test]
    async fn device_deleting() {
        let dir = "device_deleting".to_owned();
        let (_account, store) = get_loaded_store(dir.clone()).await;
        let device = get_device();

        let changes = Changes {
            devices: DeviceChanges { changed: vec![device.clone()], ..Default::default() },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let changes = Changes {
            devices: DeviceChanges { deleted: vec![device.clone()], ..Default::default() },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();
        drop(store);

        let store = IndexeddbStore::open_with_name(dir).await.expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store.get_device(device.user_id(), device.device_id()).await.unwrap();

        assert!(loaded_device.is_none());
    }

    #[async_test]
    async fn user_saving() {
        let dir = "user_saving".to_owned();

        let user_id = user_id!("@example:localhost");
        let device_id: &DeviceId = "WSKKLTJZCL".into();

        let store = IndexeddbStore::open_with_name(dir.clone()).await.expect("Can't create store");

        let account = ReadOnlyAccount::new(&user_id, device_id);

        store.save_account(account.clone()).await.expect("Can't save account");

        let own_identity = get_own_identity();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![own_identity.clone().into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.expect("Can't save identity");

        drop(store);

        let store = IndexeddbStore::open_with_name(dir).await.expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_user = store.get_user_identity(own_identity.user_id()).await.unwrap().unwrap();

        assert_eq!(loaded_user.master_key(), own_identity.master_key());
        assert_eq!(loaded_user.self_signing_key(), own_identity.self_signing_key());
        assert_eq!(loaded_user, own_identity.clone().into());

        let other_identity = get_other_identity();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![other_identity.clone().into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let loaded_user = store.get_user_identity(other_identity.user_id()).await.unwrap().unwrap();

        assert_eq!(loaded_user.master_key(), other_identity.master_key());
        assert_eq!(loaded_user.self_signing_key(), other_identity.self_signing_key());
        assert_eq!(loaded_user, other_identity.into());

        own_identity.mark_as_verified();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![own_identity.into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();
        let loaded_user = store.get_user_identity(&user_id).await.unwrap().unwrap();
        assert!(loaded_user.own().unwrap().is_verified())
    }

    #[async_test]
    async fn private_identity_saving() {
        let dir = "private_identity_saving".to_owned();
        let (_, store) = get_loaded_store(dir).await;
        assert!(store.load_identity().await.unwrap().is_none());
        let identity = PrivateCrossSigningIdentity::new(alice_id()).await;

        let changes = Changes { private_identity: Some(identity.clone()), ..Default::default() };

        store.save_changes(changes).await.unwrap();
        let loaded_identity = store.load_identity().await.unwrap().unwrap();
        assert_eq!(identity.user_id(), loaded_identity.user_id());
    }

    #[async_test]
    async fn olm_hash_saving() {
        let dir = "olm_hash_saving".to_owned();
        let (_, store) = get_loaded_store(dir).await;

        let hash =
            OlmMessageHash { sender_key: "test_sender".to_owned(), hash: "test_hash".to_owned() };

        let mut changes = Changes::default();
        changes.message_hashes.push(hash.clone());

        assert!(!store.is_message_known(&hash).await.unwrap());
        store.save_changes(changes).await.unwrap();
        assert!(store.is_message_known(&hash).await.unwrap());
    }

    #[async_test]
    async fn key_request_saving() {
        let dir = "key_request_saving".to_owned();
        let (account, store) = get_loaded_store(dir).await;

        let id = Uuid::new_v4();
        let info: SecretInfo = RequestedKeyInfo::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id!("!test:localhost"),
            "test_sender_key".to_string(),
            "test_session_id".to_string(),
        )
        .into();

        let request = GossipRequest {
            request_recipient: account.user_id().to_owned(),
            request_id: id,
            info: info.clone(),
            sent_out: false,
        };

        assert!(store.get_outgoing_secret_requests(id).await.unwrap().is_none());

        let mut changes = Changes::default();
        changes.key_requests.push(request.clone());
        store.save_changes(changes).await.unwrap();

        let request = Some(request);

        let stored_request = store.get_outgoing_secret_requests(id).await.unwrap();
        assert_eq!(request, stored_request);

        let stored_request = store.get_secret_request_by_info(&info).await.unwrap();
        assert_eq!(request, stored_request);
        assert!(!store.get_unsent_secret_requests().await.unwrap().is_empty());

        let request = GossipRequest {
            request_recipient: account.user_id().to_owned(),
            request_id: id,
            info: info.clone(),
            sent_out: true,
        };

        let mut changes = Changes::default();
        changes.key_requests.push(request.clone());
        store.save_changes(changes).await.unwrap();

        assert!(store.get_unsent_secret_requests().await.unwrap().is_empty());
        let stored_request = store.get_outgoing_secret_requests(id).await.unwrap();
        assert_eq!(Some(request), stored_request);

        store.delete_outgoing_secret_requests(id).await.unwrap();

        let stored_request = store.get_outgoing_secret_requests(id).await.unwrap();
        assert_eq!(None, stored_request);

        let stored_request = store.get_secret_request_by_info(&info).await.unwrap();
        assert_eq!(None, stored_request);
        assert!(store.get_unsent_secret_requests().await.unwrap().is_empty());
    }
}
