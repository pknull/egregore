use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use crate::error::{EgreError, Result};
use crate::identity::PublicId;

use super::{parse_peer_row, parse_rfc3339, AddressPeer, FeedStore, PeerRecord};

fn parse_address_peer_row(row: &rusqlite::Row) -> rusqlite::Result<AddressPeer> {
    let address: String = row.get(0)?;
    let public_id: Option<String> = row.get(1)?;
    let last_connected_str: Option<String> = row.get(2)?;
    let last_synced_str: Option<String> = row.get(3)?;
    Ok(AddressPeer {
        address,
        public_id,
        last_connected: last_connected_str.and_then(|s| parse_rfc3339(&s)),
        last_synced: last_synced_str.and_then(|s| parse_rfc3339(&s)),
    })
}

impl FeedStore {
    // ---- Known peer operations (known_peers table) ----

    pub fn insert_peer(&self, record: &PeerRecord) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO known_peers (public_id, address, nickname, private, authorized, first_seen, last_connected, last_synced)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(public_id) DO UPDATE SET
                address = COALESCE(excluded.address, known_peers.address),
                nickname = COALESCE(excluded.nickname, known_peers.nickname)",
            params![
                record.public_id.0,
                record.address,
                record.nickname,
                record.private as i32,
                record.authorized as i32,
                record.first_seen.to_rfc3339(),
                record.last_connected.map(|dt| dt.to_rfc3339()),
                record.last_synced.map(|dt| dt.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn get_peer(&self, public_id: &PublicId) -> Result<Option<PeerRecord>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT public_id, address, nickname, private, authorized, first_seen, last_connected, last_synced
             FROM known_peers WHERE public_id = ?1",
            params![public_id.0],
            |row| Ok(parse_peer_row(row)),
        )
        .optional()?
        .transpose()
    }

    /// List peers. If `include_private` is false, only non-private peers are returned.
    pub fn list_peers(&self, include_private: bool) -> Result<Vec<PeerRecord>> {
        let conn = self.conn();
        let sql = if include_private {
            "SELECT public_id, address, nickname, private, authorized, first_seen, last_connected, last_synced
             FROM known_peers WHERE authorized = 1"
        } else {
            "SELECT public_id, address, nickname, private, authorized, first_seen, last_connected, last_synced
             FROM known_peers WHERE authorized = 1 AND private = 0"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| Ok(parse_peer_row(row)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().collect()
    }

    pub fn remove_peer(&self, public_id: &PublicId) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM known_peers WHERE public_id = ?1",
            params![public_id.0],
        )?;
        Ok(())
    }

    pub fn update_peer_connected(&self, public_id: &PublicId) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE known_peers SET last_connected = ?1 WHERE public_id = ?2",
            params![Utc::now().to_rfc3339(), public_id.0],
        )?;
        Ok(())
    }

    pub fn update_peer_synced(&self, public_id: &PublicId) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE known_peers SET last_synced = ?1 WHERE public_id = ?2",
            params![Utc::now().to_rfc3339(), public_id.0],
        )?;
        Ok(())
    }

    pub fn update_peer_private(&self, public_id: &PublicId, private: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE known_peers SET private = ?1 WHERE public_id = ?2",
            params![private as i32, public_id.0],
        )?;
        Ok(())
    }

    pub fn is_peer_authorized(&self, public_id: &PublicId) -> Result<bool> {
        let conn = self.conn();
        conn.query_row(
            "SELECT authorized FROM known_peers WHERE public_id = ?1",
            params![public_id.0],
            |row| {
                let auth: i32 = row.get(0)?;
                Ok(auth != 0)
            },
        )
        .optional()
        .map(|opt| opt.unwrap_or(false))
        .map_err(EgreError::from)
    }

    // ---- Address peer operations (peers table) ----

    pub fn insert_address_peer(&self, address: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO peers (address) VALUES (?1)",
            params![address],
        )?;
        Ok(())
    }

    pub fn get_address_peer(&self, address: &str) -> Result<Option<AddressPeer>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT address, public_id, last_connected, last_synced FROM peers WHERE address = ?1",
            params![address],
            parse_address_peer_row,
        )
        .optional()
        .map_err(EgreError::from)
    }

    pub fn list_address_peers(&self) -> Result<Vec<AddressPeer>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT address, public_id, last_connected, last_synced FROM peers")?;
        let rows = stmt
            .query_map([], parse_address_peer_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn remove_address_peer(&self, address: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM peers WHERE address = ?1", params![address])?;
        Ok(())
    }

    pub fn update_address_peer_synced(&self, address: &str, public_id: &str) -> Result<()> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE peers SET public_id = ?1, last_connected = ?2, last_synced = ?2 WHERE address = ?3",
            params![public_id, now, address],
        )?;
        Ok(())
    }

    /// Union of addresses from `peers` table and `known_peers` table, deduplicated.
    pub fn list_all_syncable_addresses(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT address FROM peers WHERE address IS NOT NULL
             UNION
             SELECT address FROM known_peers WHERE authorized = 1 AND address IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let address: String = row.get(0)?;
                Ok(address)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_crud() {
        let store = FeedStore::open_memory().unwrap();
        let pub_id = PublicId("@test.ed25519".to_string());

        let record = PeerRecord {
            public_id: pub_id.clone(),
            address: Some("127.0.0.1:7655".to_string()),
            nickname: Some("test-node".to_string()),
            private: false,
            authorized: true,
            first_seen: Utc::now(),
            last_connected: None,
            last_synced: None,
        };

        store.insert_peer(&record).unwrap();

        let retrieved = store.get_peer(&pub_id).unwrap().unwrap();
        assert_eq!(retrieved.public_id, pub_id);
        assert_eq!(retrieved.nickname.as_deref(), Some("test-node"));
        assert!(!retrieved.private);
        assert!(retrieved.authorized);

        store.remove_peer(&pub_id).unwrap();
        assert!(store.get_peer(&pub_id).unwrap().is_none());
    }

    #[test]
    fn peer_list_respects_privacy() {
        let store = FeedStore::open_memory().unwrap();

        let public_peer = PeerRecord {
            public_id: PublicId("@pub.ed25519".to_string()),
            address: None,
            nickname: None,
            private: false,
            authorized: true,
            first_seen: Utc::now(),
            last_connected: None,
            last_synced: None,
        };
        let private_peer = PeerRecord {
            public_id: PublicId("@priv.ed25519".to_string()),
            address: None,
            nickname: None,
            private: true,
            authorized: true,
            first_seen: Utc::now(),
            last_connected: None,
            last_synced: None,
        };

        store.insert_peer(&public_peer).unwrap();
        store.insert_peer(&private_peer).unwrap();

        let all = store.list_peers(true).unwrap();
        assert_eq!(all.len(), 2);

        let public_only = store.list_peers(false).unwrap();
        assert_eq!(public_only.len(), 1);
        assert_eq!(public_only[0].public_id.0, "@pub.ed25519");
    }

    #[test]
    fn peer_authorization() {
        let store = FeedStore::open_memory().unwrap();
        let pub_id = PublicId("@auth.ed25519".to_string());

        assert!(!store.is_peer_authorized(&pub_id).unwrap());

        store
            .insert_peer(&PeerRecord {
                public_id: pub_id.clone(),
                address: None,
                nickname: None,
                private: false,
                authorized: true,
                first_seen: Utc::now(),
                last_connected: None,
                last_synced: None,
            })
            .unwrap();

        assert!(store.is_peer_authorized(&pub_id).unwrap());
    }

    #[test]
    fn peer_update_timestamps() {
        let store = FeedStore::open_memory().unwrap();
        let pub_id = PublicId("@ts.ed25519".to_string());

        store
            .insert_peer(&PeerRecord {
                public_id: pub_id.clone(),
                address: None,
                nickname: None,
                private: false,
                authorized: true,
                first_seen: Utc::now(),
                last_connected: None,
                last_synced: None,
            })
            .unwrap();

        store.update_peer_connected(&pub_id).unwrap();
        let p = store.get_peer(&pub_id).unwrap().unwrap();
        assert!(p.last_connected.is_some());

        store.update_peer_synced(&pub_id).unwrap();
        let p = store.get_peer(&pub_id).unwrap().unwrap();
        assert!(p.last_synced.is_some());
    }

    // ---- Address peer tests ----

    #[test]
    fn address_peer_crud() {
        let store = FeedStore::open_memory().unwrap();
        let addr = "127.0.0.1:7655";

        store.insert_address_peer(addr).unwrap();
        let peer = store.get_address_peer(addr).unwrap().unwrap();
        assert_eq!(peer.address, addr);
        assert!(peer.public_id.is_none());

        store.remove_address_peer(addr).unwrap();
        assert!(store.get_address_peer(addr).unwrap().is_none());
    }

    #[test]
    fn address_peer_insert_idempotent() {
        let store = FeedStore::open_memory().unwrap();
        let addr = "127.0.0.1:7655";

        store.insert_address_peer(addr).unwrap();
        store.insert_address_peer(addr).unwrap(); // no error
        assert_eq!(store.list_address_peers().unwrap().len(), 1);
    }

    #[test]
    fn address_peer_update_synced() {
        let store = FeedStore::open_memory().unwrap();
        let addr = "127.0.0.1:7655";
        store.insert_address_peer(addr).unwrap();

        store
            .update_address_peer_synced(addr, "@test.ed25519")
            .unwrap();
        let peer = store.get_address_peer(addr).unwrap().unwrap();
        assert_eq!(peer.public_id.as_deref(), Some("@test.ed25519"));
        assert!(peer.last_synced.is_some());
        assert!(peer.last_connected.is_some());
    }

    #[test]
    fn list_all_syncable_addresses_deduplicates() {
        let store = FeedStore::open_memory().unwrap();
        let addr = "127.0.0.1:7655";

        // Add to both tables
        store.insert_address_peer(addr).unwrap();
        store
            .insert_peer(&PeerRecord {
                public_id: PublicId("@dup.ed25519".to_string()),
                address: Some(addr.to_string()),
                nickname: None,
                private: false,
                authorized: true,
                first_seen: Utc::now(),
                last_connected: None,
                last_synced: None,
            })
            .unwrap();

        let addrs = store.list_all_syncable_addresses().unwrap();
        assert_eq!(addrs.len(), 1, "UNION should deduplicate");
    }

    #[test]
    fn list_all_syncable_addresses_merges_both_tables() {
        let store = FeedStore::open_memory().unwrap();

        store.insert_address_peer("10.0.0.1:7655").unwrap();
        store
            .insert_peer(&PeerRecord {
                public_id: PublicId("@known.ed25519".to_string()),
                address: Some("10.0.0.2:7655".to_string()),
                nickname: None,
                private: false,
                authorized: true,
                first_seen: Utc::now(),
                last_connected: None,
                last_synced: None,
            })
            .unwrap();

        let mut addrs = store.list_all_syncable_addresses().unwrap();
        addrs.sort();
        assert_eq!(addrs, vec!["10.0.0.1:7655", "10.0.0.2:7655"]);
    }
}
