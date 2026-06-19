#![allow(missing_docs)]

use kuatia_storage::mem_store::InMemoryStore;

async fn new_store() -> InMemoryStore {
    InMemoryStore::new()
}

kuatia_storage::store_tests!(new_store);
