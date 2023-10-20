// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(unknown_lints)]
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![allow(clippy::drain_collect)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG
// Disallow usage of `unwrap()`.
#![warn(clippy::unwrap_used)]

//! This crate is responsible for durably storing and modifying the catalog contents.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::num::NonZeroI64;
use std::time::Duration;
use uuid::Uuid;

use mz_stash::DebugStashFactory;

pub use crate::error::{CatalogError, DurableCatalogError};
pub use crate::objects::{
    Cluster, ClusterConfig, ClusterReplica, ClusterVariant, ClusterVariantManaged, Comment,
    Database, DefaultPrivilege, Item, ReplicaConfig, ReplicaLocation, Role, Schema,
    SystemConfiguration, SystemObjectMapping, TimelineTimestamp,
};
use crate::objects::{IntrospectionSourceIndex, Snapshot};
use crate::persist::{PersistCatalogState, PersistHandle};
use crate::stash::{Connection, DebugOpenableConnection, OpenableConnection};
pub use crate::stash::{
    StashConfig, ALL_COLLECTIONS, AUDIT_LOG_COLLECTION, CLUSTER_COLLECTION,
    CLUSTER_INTROSPECTION_SOURCE_INDEX_COLLECTION, CLUSTER_REPLICA_COLLECTION, COMMENTS_COLLECTION,
    CONFIG_COLLECTION, DATABASES_COLLECTION, DEFAULT_PRIVILEGES_COLLECTION,
    ID_ALLOCATOR_COLLECTION, ITEM_COLLECTION, ROLES_COLLECTION, SCHEMAS_COLLECTION,
    SETTING_COLLECTION, STORAGE_USAGE_COLLECTION, SYSTEM_CONFIGURATION_COLLECTION,
    SYSTEM_GID_MAPPING_COLLECTION, SYSTEM_PRIVILEGES_COLLECTION, TIMESTAMP_COLLECTION,
};
pub use crate::transaction::Transaction;
use crate::transaction::TransactionBatch;
use mz_audit_log::{VersionedEvent, VersionedStorageUsage};
use mz_controller_types::{ClusterId, ReplicaId};
use mz_ore::collections::CollectionExt;
use mz_ore::now::NowFn;
use mz_persist_client::PersistClient;
use mz_repr::adt::mz_acl_item::MzAclItem;
use mz_repr::role_id::RoleId;
use mz_repr::GlobalId;
use mz_storage_types::sources::Timeline;

mod stash;
mod transaction;

pub mod builtin;
mod error;
pub mod initialize;
pub mod objects;
mod persist;

pub const DATABASE_ID_ALLOC_KEY: &str = "database";
pub const SCHEMA_ID_ALLOC_KEY: &str = "schema";
pub const USER_ITEM_ALLOC_KEY: &str = "user";
pub const SYSTEM_ITEM_ALLOC_KEY: &str = "system";
pub const USER_ROLE_ID_ALLOC_KEY: &str = "user_role";
pub const USER_CLUSTER_ID_ALLOC_KEY: &str = "user_compute";
pub const SYSTEM_CLUSTER_ID_ALLOC_KEY: &str = "system_compute";
pub const USER_REPLICA_ID_ALLOC_KEY: &str = "replica";
pub const SYSTEM_REPLICA_ID_ALLOC_KEY: &str = "system_replica";
pub const AUDIT_LOG_ID_ALLOC_KEY: &str = "auditlog";
pub const STORAGE_USAGE_ID_ALLOC_KEY: &str = "storage_usage";

#[derive(Clone, Debug)]
pub struct BootstrapArgs {
    pub default_cluster_replica_size: String,
    pub builtin_cluster_replica_size: String,
    pub bootstrap_role: Option<String>,
}

pub type Epoch = NonZeroI64;

/// An API for opening a durable catalog state.
///
/// If a catalog is not opened, then resources should be release via [`Self::expire`].
#[async_trait]
pub trait OpenableDurableCatalogState<D: DurableCatalogState>: Debug + Send {
    /// Opens the catalog in a mode that accepts and buffers all writes,
    /// but never durably commits them. This is used to check and see if
    /// opening the catalog would be successful, without making any durable
    /// changes.
    ///
    /// Will return an error in the following scenarios:
    ///   - Catalog initialization fails.
    ///   - Catalog migrations fail.
    async fn open_savepoint(
        mut self,
        now: NowFn,
        bootstrap_args: &BootstrapArgs,
        deploy_generation: Option<u64>,
    ) -> Result<D, CatalogError>;

    /// Opens the catalog in read only mode. All mutating methods
    /// will return an error.
    ///
    /// If the catalog is uninitialized or requires a migrations, then
    /// it will fail to open in read only mode.
    async fn open_read_only(
        mut self,
        now: NowFn,
        bootstrap_args: &BootstrapArgs,
    ) -> Result<D, CatalogError>;

    /// Opens the catalog in a writeable mode. Optionally initializes the
    /// catalog, if it has not been initialized, and perform any migrations
    /// needed.
    async fn open(
        mut self,
        now: NowFn,
        bootstrap_args: &BootstrapArgs,
        deploy_generation: Option<u64>,
    ) -> Result<D, CatalogError>;

    /// Reports if the catalog state has been initialized.
    async fn is_initialized(&mut self) -> Result<bool, CatalogError>;

    /// Get the deployment generation of this instance.
    async fn get_deployment_generation(&mut self) -> Result<Option<u64>, CatalogError>;

    /// Politely releases all external resources that can only be released in an async context.
    async fn expire(self);
}

// TODO(jkosh44) No method should take &mut self, but due to stash implementations we need it.
/// A read only API for the durable catalog state.
#[async_trait]
pub trait ReadOnlyDurableCatalogState: Debug + Send {
    /// Returns the epoch of the current durable catalog state. The epoch acts as
    /// a fencing token to prevent split brain issues across two
    /// [`DurableCatalogState`]s. When a new [`DurableCatalogState`] opens the
    /// catalog, it will increment the epoch by one (or initialize it to some
    /// value if there's no existing epoch) and store the value in memory. It's
    /// guaranteed that no two [`DurableCatalogState`]s will return the same value
    /// for their epoch.
    ///
    /// NB: We may remove this in later iterations of Pv2.
    fn epoch(&mut self) -> Epoch;

    /// Politely releases all external resources that can only be released in an async context.
    async fn expire(self: Box<Self>);

    /// Returns the version of Materialize that last wrote to the catalog.
    ///
    /// If the catalog is uninitialized this will return None.
    async fn get_catalog_content_version(&mut self) -> Result<Option<String>, CatalogError>;

    /// Get all clusters.
    async fn get_clusters(&mut self) -> Result<Vec<Cluster>, CatalogError>;

    /// Get all cluster replicas.
    async fn get_cluster_replicas(&mut self) -> Result<Vec<ClusterReplica>, CatalogError>;

    /// Get all databases.
    async fn get_databases(&mut self) -> Result<Vec<Database>, CatalogError>;

    /// Get all schemas.
    async fn get_schemas(&mut self) -> Result<Vec<Schema>, CatalogError>;

    /// Get all system items.
    async fn get_system_items(&mut self) -> Result<Vec<SystemObjectMapping>, CatalogError>;

    /// Get all introspection source indexes.
    ///
    /// Returns (index-name, global-id).
    async fn get_introspection_source_indexes(
        &mut self,
        cluster_id: ClusterId,
    ) -> Result<BTreeMap<String, GlobalId>, CatalogError>;

    /// Get all roles.
    async fn get_roles(&mut self) -> Result<Vec<Role>, CatalogError>;

    /// Get all default privileges.
    async fn get_default_privileges(&mut self) -> Result<Vec<DefaultPrivilege>, CatalogError>;

    /// Get all system privileges.
    async fn get_system_privileges(&mut self) -> Result<Vec<MzAclItem>, CatalogError>;

    /// Get all system configurations.
    async fn get_system_configurations(&mut self)
        -> Result<Vec<SystemConfiguration>, CatalogError>;

    /// Get all comments.
    async fn get_comments(&mut self) -> Result<Vec<Comment>, CatalogError>;

    /// Get all timelines and their persisted timestamps.
    // TODO(jkosh44) This should be removed once the timestamp oracle is extracted.
    async fn get_timestamps(&mut self) -> Result<Vec<TimelineTimestamp>, CatalogError>;

    /// Get the persisted timestamp of a timeline.
    // TODO(jkosh44) This should be removed once the timestamp oracle is extracted.
    async fn get_timestamp(
        &mut self,
        timeline: &Timeline,
    ) -> Result<Option<mz_repr::Timestamp>, CatalogError>;

    /// Get all audit log events.
    async fn get_audit_logs(&mut self) -> Result<Vec<VersionedEvent>, CatalogError>;

    /// Get the next ID of `id_type`, without allocating it.
    async fn get_next_id(&mut self, id_type: &str) -> Result<u64, CatalogError>;

    /// Get the next system replica id without allocating it.
    async fn get_next_system_replica_id(&mut self) -> Result<u64, CatalogError> {
        self.get_next_id(SYSTEM_REPLICA_ID_ALLOC_KEY).await
    }

    /// Get the next user replica id without allocating it.
    async fn get_next_user_replica_id(&mut self) -> Result<u64, CatalogError> {
        self.get_next_id(USER_REPLICA_ID_ALLOC_KEY).await
    }

    /// Get a snapshot of the catalog.
    async fn snapshot(&mut self) -> Result<Snapshot, CatalogError>;

    // TODO(jkosh44) Implement this for the catalog debug tool.
    /*    /// Dumps the entire catalog contents in human readable JSON.
    async fn dump(&self) -> Result<String, Error>;*/
}

/// A read-write API for the durable catalog state.
#[async_trait]
pub trait DurableCatalogState: ReadOnlyDurableCatalogState {
    /// Returns true if the catalog is opened in read only mode, false otherwise.
    fn is_read_only(&self) -> bool;

    /// Creates a new durable catalog state transaction.
    async fn transaction(&mut self) -> Result<Transaction, CatalogError>;

    /// Commits a durable catalog state transaction.
    async fn commit_transaction(&mut self, txn_batch: TransactionBatch)
        -> Result<(), CatalogError>;

    /// Confirms that this catalog is connected as the current leader.
    ///
    /// NB: We may remove this in later iterations of Pv2.
    async fn confirm_leadership(&mut self) -> Result<(), CatalogError>;

    /// Set's the connection timeout for the underlying durable store.
    async fn set_connect_timeout(&mut self, connect_timeout: Duration);

    /// Persist the version of Materialize that last wrote to the catalog.
    async fn set_catalog_content_version(&mut self, new_version: &str) -> Result<(), CatalogError>;

    /// Gets all storage usage events and permanently deletes from the catalog those
    /// that happened more than the retention period ago from boot_ts.
    async fn get_and_prune_storage_usage(
        &mut self,
        retention_period: Option<Duration>,
        boot_ts: mz_repr::Timestamp,
    ) -> Result<Vec<VersionedStorageUsage>, CatalogError>;

    /// Persist system items.
    async fn set_system_items(
        &mut self,
        mappings: Vec<SystemObjectMapping>,
    ) -> Result<(), CatalogError>;

    /// Persist introspection source indexes.
    ///
    /// `mappings` has the format (cluster-id, index-name, global-id).
    ///
    /// Panics if the provided id is not a system id.
    async fn set_introspection_source_indexes(
        &mut self,
        mappings: Vec<IntrospectionSourceIndex>,
    ) -> Result<(), CatalogError>;

    /// Persist the configuration of a replica.
    /// This accepts only one item, as we currently use this only for the default cluster
    async fn set_replica_config(
        &mut self,
        replica_id: ReplicaId,
        cluster_id: ClusterId,
        name: String,
        config: ReplicaConfig,
        owner_id: RoleId,
    ) -> Result<(), CatalogError>;

    /// Persist new global timestamp for a timeline.
    async fn set_timestamp(
        &mut self,
        timeline: &Timeline,
        timestamp: mz_repr::Timestamp,
    ) -> Result<(), CatalogError>;

    /// Persist the deployment generation of this instance.
    async fn set_deploy_generation(&mut self, deploy_generation: u64) -> Result<(), CatalogError>;

    /// Allocates and returns `amount` IDs of `id_type`.
    async fn allocate_id(&mut self, id_type: &str, amount: u64) -> Result<Vec<u64>, CatalogError>;

    /// Allocates and returns `amount` system [`GlobalId`]s.
    async fn allocate_system_ids(&mut self, amount: u64) -> Result<Vec<GlobalId>, CatalogError> {
        let id = self.allocate_id(SYSTEM_ITEM_ALLOC_KEY, amount).await?;

        Ok(id.into_iter().map(GlobalId::System).collect())
    }

    /// Allocates and returns a user [`GlobalId`].
    async fn allocate_user_id(&mut self) -> Result<GlobalId, CatalogError> {
        let id = self.allocate_id(USER_ITEM_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(GlobalId::User(id))
    }

    /// Allocates and returns a system [`ClusterId`].
    async fn allocate_system_cluster_id(&mut self) -> Result<ClusterId, CatalogError> {
        let id = self.allocate_id(SYSTEM_CLUSTER_ID_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(ClusterId::System(id))
    }

    /// Allocates and returns a user [`ClusterId`].
    async fn allocate_user_cluster_id(&mut self) -> Result<ClusterId, CatalogError> {
        let id = self.allocate_id(USER_CLUSTER_ID_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(ClusterId::User(id))
    }

    /// Allocates and returns a user [`ReplicaId`].
    async fn allocate_user_replica_id(&mut self) -> Result<ReplicaId, CatalogError> {
        let id = self.allocate_id(USER_REPLICA_ID_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(ReplicaId::User(id))
    }
}

/// Creates a openable durable catalog state implemented using the stash.
pub fn stash_backed_catalog_state(
    config: StashConfig,
) -> impl OpenableDurableCatalogState<Connection> {
    OpenableConnection::new(config)
}

/// Creates an openable debug durable catalog state implemented using the stash that is meant to be
/// used in tests.
pub fn debug_stash_backed_catalog_state(
    debug_stash_factory: &DebugStashFactory,
) -> impl OpenableDurableCatalogState<Connection> + '_ {
    DebugOpenableConnection::new(debug_stash_factory)
}

/// Creates an openable durable catalog state implemented using persist.
pub async fn persist_backed_catalog_state(
    persist_client: PersistClient,
    environment_id: Uuid,
) -> impl OpenableDurableCatalogState<PersistCatalogState> {
    PersistHandle::new(persist_client, environment_id).await
}

pub fn debug_bootstrap_args() -> BootstrapArgs {
    BootstrapArgs {
        default_cluster_replica_size: "1".into(),
        builtin_cluster_replica_size: "1".into(),
        bootstrap_role: None,
    }
}
