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
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
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

use mz_stash::upgrade::json_to_proto::migrate_json_to_proto;
use mz_stash::upgrade::json_to_proto::test_helpers::initialize_json_stash;
use rand::Rng;
use tokio_postgres::Config;

// Note: This test exists in it's own integration test file because it uses failpoints. Failpoints
// allow us to inject failures at specific points in our code, but they're set for the entire process
// so when run as unit tests it interferes with the other running tests. Whereas integration tests
// run in their own processes, which gives us the necessary isolation.

#[mz_ore::test(tokio::test)]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `TLS_client_method` on OS `linux`
async fn test_failures() {
    // Connect to Cockroach.
    let tls = mz_postgres_util::make_tls(&Config::new()).unwrap();
    let connstr = std::env::var("COCKROACH_URL").expect("COCKROACH_URL must be set");
    let (mut client, connection) = tokio_postgres::connect(&connstr, tls.clone())
        .await
        .expect("able to connect");
    mz_ore::task::spawn(|| "tokio-postgres stash connection", async move {
        if let Err(e) = connection.await {
            tracing::error!("postgres stash connection error: {}", e);
        }
    });

    // Create a schema for our test.
    let seed: u32 = rand::thread_rng().gen();
    println!("Using Seed {seed}");
    let schema = format!("stash_test_{seed}");

    client
        .execute(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"), &[])
        .await
        .unwrap();
    client
        .execute(&format!("SET search_path TO {schema}"), &[])
        .await
        .unwrap();

    // Initialize the Stash.
    let epoch = initialize_json_stash(&client, schema).await;

    // Fail once at each point. The migration should have enough retries and recovery logic to
    // still succeed overall.
    fail::cfg("stash_proto_create_table", "1*return(1)->off").unwrap();
    fail::cfg("stash_proto_migrate", "1*return(1)->off").unwrap();
    fail::cfg("stash_proto_swap_table", "1*return(1)->off").unwrap();

    // Migrate the Stash to protobuf.
    migrate_json_to_proto(&mut client, epoch)
        .await
        .expect("migration to succeed");
}
