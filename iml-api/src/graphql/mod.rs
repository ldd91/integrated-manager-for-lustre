// Copyright (c) 2020 DDN. All rights reserved.
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file.

mod filesystem;
mod snapshot;
mod stratagem;
mod task;

use crate::error::ImlApiError;
use chrono::{DateTime, Utc};
use futures::{
    future::{self, join_all},
    TryFutureExt, TryStreamExt,
};
use iml_postgres::{fqdn_by_host_id, sqlx, PgPool};
use iml_rabbit::{ImlRabbitError, Pool};
use iml_wire_types::{
    db::{LogMessageRecord, LustreFid, ServerProfileRecord, TargetRecord},
    graphql::{ServerProfile, ServerProfileInput},
    logs::{LogResponse, Meta},
    task::Task,
    Command, EndpointName, FsType, Job, LogMessage, LogSeverity, MessageClass, SortDir,
};
use itertools::Itertools;
use juniper::{
    http::{graphiql::graphiql_source, GraphQLRequest},
    EmptySubscription, FieldError, RootNode, Value,
};
use std::{
    collections::{HashMap, HashSet},
    convert::{Infallible, TryInto},
    ops::Deref,
    str::FromStr,
    sync::Arc,
};
use warp::Filter;

#[derive(juniper::GraphQLObject)]
/// A Corosync Node found in `crm_mon`
struct CorosyncNode {
    /// The name of the node
    name: String,
    /// The id of the node as reported by `crm_mon`
    id: String,
    /// Id of the cluster this node belongs to
    cluster_id: i32,
    online: bool,
    standby: bool,
    standby_onfail: bool,
    maintenance: bool,
    pending: bool,
    unclean: bool,
    shutdown: bool,
    expected_up: bool,
    is_dc: bool,
    resources_running: i32,
    r#type: String,
}

#[derive(juniper::GraphQLObject, Hash, Eq, PartialEq)]
/// A Lustre Target and it's corresponding resource
struct TargetResource {
    /// The id of the cluster
    cluster_id: i32,
    /// The filesystems associated with this target
    fs_names: Vec<String>,
    /// The uuid of this target
    uuid: String,
    /// The name of this target
    name: String,
    /// The corosync resource id associated with this target
    resource_id: String,
    /// The current state of this target
    state: String,
    /// The list of host ids this target could possibly run on
    cluster_hosts: Vec<i32>,
}

struct BannedTargetResource {
    resource: String,
    cluster_id: i32,
    host_id: i32,
    mount_point: Option<String>,
}

#[derive(juniper::GraphQLObject)]
/// A Corosync banned resource
struct BannedResource {
    // The primary id
    id: i32,
    /// The resource name
    name: String,
    /// The id of the cluster in which the resource lives
    cluster_id: i32,
    /// The resource name
    resource: String,
    /// The node in which the resource lives
    node: String,
    /// The assigned weight of the resource
    weight: i32,
    /// Is master only
    master_only: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct SendJob<'a, T> {
    pub class_name: &'a str,
    pub args: T,
}

pub(crate) struct QueryRoot;

#[juniper::graphql_object(Context = Context)]
impl QueryRoot {
    fn stratagem(&self) -> stratagem::StratagemQuery {
        stratagem::StratagemQuery
    }
    fn task(&self) -> task::TaskQuery {
        task::TaskQuery
    }
    fn snapshot(&self) -> snapshot::SnapshotQuery {
        snapshot::SnapshotQuery
    }
    /// Given a host id, try to find the matching corosync node name
    #[graphql(arguments(host_id(description = "The id to search on")))]
    async fn corosync_node_name_by_host(
        context: &Context,
        host_id: i32,
    ) -> juniper::FieldResult<Option<String>> {
        let x = sqlx::query!(
            r#"
                SELECT (nmh.corosync_node_id).name AS "name!" FROM corosync_node_managed_host nmh
                WHERE host_id = $1
            "#,
            host_id
        )
        .fetch_optional(&context.pg_pool)
        .await?
        .map(|x| x.name);

        Ok(x)
    }
    #[graphql(arguments(
        limit(description = "optional paging limit, defaults to all rows"),
        offset(description = "Offset into items, defaults to 0"),
        dir(description = "Sort direction, defaults to asc")
    ))]
    async fn corosync_nodes(
        context: &Context,
        limit: Option<i32>,
        offset: Option<i32>,
        dir: Option<SortDir>,
    ) -> juniper::FieldResult<Vec<CorosyncNode>> {
        let dir = dir.unwrap_or_default();

        let xs = sqlx::query_as!(
            CorosyncNode,
            r#"
                SELECT
                (n.id).name AS "name!",
                (n.id).id AS "id!",
                cluster_id,
                online,
                standby,
                standby_onfail,
                maintenance,
                pending,
                unclean,
                shutdown,
                expected_up,
                is_dc,
                resources_running,
                type
                FROM corosync_node n
                ORDER BY
                    CASE WHEN $1 = 'ASC' THEN n.id END ASC,
                    CASE WHEN $1 = 'DESC' THEN n.id END DESC
                OFFSET $2 LIMIT $3"#,
            dir.deref(),
            offset.unwrap_or(0) as i64,
            limit.map(|x| x as i64),
        )
        .fetch_all(&context.pg_pool)
        .await?;

        Ok(xs)
    }

    #[graphql(arguments(
        limit(description = "optional paging limit, defaults to all rows"),
        offset(description = "Offset into items, defaults to 0"),
        dir(description = "Sort direction, defaults to asc"),
        fs_name(description = "Targets associated with the specified filesystem"),
        exclude_unmounted(description = "Exclude unmounted targets, defaults to false"),
    ))]
    /// Fetch the list of known targets
    async fn targets(
        context: &Context,
        limit: Option<i32>,
        offset: Option<i32>,
        dir: Option<SortDir>,
        fs_name: Option<String>,
        exclude_unmounted: Option<bool>,
    ) -> juniper::FieldResult<Vec<TargetRecord>> {
        let dir = dir.unwrap_or_default();

        if let Some(ref fs_name) = fs_name {
            let _ = fs_id_by_name(&context.pg_pool, &fs_name).await?;
        }

        let xs: Vec<TargetRecord> = sqlx::query_as!(
            TargetRecord,
            r#"
                SELECT id, state, name, active_host_id, host_ids, filesystems, uuid, mount_path, dev_path, fs_type as "fs_type: FsType" from target t
                ORDER BY
                    CASE WHEN $3 = 'ASC' THEN t.name END ASC,
                    CASE WHEN $3 = 'DESC' THEN t.name END DESC
                OFFSET $1 LIMIT $2"#,
            offset.unwrap_or(0) as i64,
            limit.map(|x| x as i64),
            dir.deref()
        )
        .fetch_all(&context.pg_pool)
        .await?
        .into_iter()
        .filter(|x| match &fs_name {
            Some(fs) => x.filesystems.contains(&fs),
            None => true,
        })
        .filter(|x| match exclude_unmounted {
            Some(true) => x.state != "unmounted",
            Some(false) | None => true,
        })
        .collect();

        let target_resources = get_fs_target_resources(&context.pg_pool, None).await?;

        let xs: Vec<TargetRecord> = xs
            .into_iter()
            .map(|mut x| {
                let resource = target_resources
                    .iter()
                    .find(|resource| resource.name == x.name);

                if let Some(resource) = resource {
                    x.host_ids = resource.cluster_hosts.clone();
                }

                x
            })
            .collect();

        Ok(xs)
    }

    /// Given a `fs_name`, produce a list of `TargetResource`.
    /// Each `TargetResource` will list the host ids it's capable of
    /// running on, taking bans into account.
    #[graphql(arguments(fs_name(description = "The filesystem to list `TargetResource`s for"),))]
    async fn get_fs_target_resources(
        context: &Context,
        fs_name: Option<String>,
    ) -> juniper::FieldResult<Vec<TargetResource>> {
        if let Some(ref fs_name) = fs_name {
            let _ = fs_id_by_name(&context.pg_pool, &fs_name).await?;
        }
        let xs = get_fs_target_resources(&context.pg_pool, fs_name).await?;

        Ok(xs)
    }

    async fn get_banned_resources(context: &Context) -> juniper::FieldResult<Vec<BannedResource>> {
        let xs = get_banned_resources(&context.pg_pool).await?;

        Ok(xs)
    }

    /// Given a `fs_name`, produce a distinct grouping
    /// of cluster nodes that make up the filesystem.
    /// This is useful to find nodes capable of running resources associated
    /// with the given `fs_name`.
    #[graphql(arguments(fs_name(description = "The filesystem to search cluster nodes for")))]
    async fn get_fs_cluster_hosts(
        context: &Context,
        fs_name: String,
    ) -> juniper::FieldResult<Vec<Vec<String>>> {
        let _ = fs_id_by_name(&context.pg_pool, &fs_name).await?;
        let xs = get_fs_cluster_hosts(&context.pg_pool, fs_name).await?;

        Ok(xs)
    }

    /// Fetch the list of commands
    #[graphql(arguments(
        limit(description = "optional paging limit, defaults to all rows"),
        offset(description = "Offset into items, defaults to 0"),
        dir(description = "Sort direction, defaults to ASC"),
        is_active(description = "Command status, active means not completed, default is true"),
        msg(description = "Substring of the command's message, null or empty matches all"),
    ))]
    async fn commands(
        context: &Context,
        limit: Option<i32>,
        offset: Option<i32>,
        dir: Option<SortDir>,
        is_active: Option<bool>,
        msg: Option<String>,
    ) -> juniper::FieldResult<Vec<Command>> {
        let dir = dir.unwrap_or_default();
        let is_completed = !is_active.unwrap_or(true);
        let commands: Vec<Command> = sqlx::query_as!(
            CommandTmpRecord,
            r#"
                SELECT
                    c.id AS id,
                    cancelled,
                    complete,
                    errored,
                    created_at,
                    array_agg(cj.job_id)::INT[] AS job_ids,
                    message
                FROM chroma_core_command c
                JOIN chroma_core_command_jobs cj ON c.id = cj.command_id
                WHERE ($4::BOOL IS NULL OR complete = $4)
                  AND ($5::TEXT IS NULL OR c.message ILIKE '%' || $5 || '%')
                GROUP BY c.id
                ORDER BY
                    CASE WHEN $3 = 'ASC' THEN c.id END ASC,
                    CASE WHEN $3 = 'DESC' THEN c.id END DESC
                OFFSET $1 LIMIT $2
            "#,
            offset.unwrap_or(0) as i64,
            limit.map(|x| x as i64),
            dir.deref(),
            is_completed,
            msg,
        )
        .fetch_all(&context.pg_pool)
        .map_ok(|xs: Vec<CommandTmpRecord>| {
            xs.into_iter().map(to_command).collect::<Vec<Command>>()
        })
        .await?;
        Ok(commands)
    }

    /// Fetch the list of commands by ids, the returned
    /// collection is guaranteed to match the input.
    /// If a command not found, `None` is returned for that index.
    #[graphql(arguments(
        limit(description = "paging limit, defaults to 20"),
        offset(description = "Offset into items, defaults to 0"),
        ids(description = "The list of command ids to fetch, ids may be empty"),
    ))]
    async fn commands_by_ids(
        context: &Context,
        limit: Option<i32>,
        offset: Option<i32>,
        ids: Vec<i32>,
    ) -> juniper::FieldResult<Vec<Option<Command>>> {
        let ids: &[i32] = &ids[..];
        let unordered_cmds: Vec<Command> = sqlx::query_as!(
            CommandTmpRecord,
            r#"
                SELECT
                    c.id AS id,
                    cancelled,
                    complete,
                    errored,
                    created_at,
                    array_agg(cj.job_id)::INT[] AS job_ids,
                    message
                FROM chroma_core_command c
                JOIN chroma_core_command_jobs cj ON c.id = cj.command_id
                WHERE (c.id = ANY ($3::INT[]))
                GROUP BY c.id
                OFFSET $1 LIMIT $2
            "#,
            offset.unwrap_or(0) as i64,
            limit.unwrap_or(20) as i64,
            ids,
        )
        .fetch_all(&context.pg_pool)
        .map_ok(|xs: Vec<CommandTmpRecord>| {
            xs.into_iter().map(to_command).collect::<Vec<Command>>()
        })
        .await?;
        let mut hm = unordered_cmds
            .into_iter()
            .map(|c| (c.id, c))
            .collect::<HashMap<i32, Command>>();
        let commands = ids
            .iter()
            .map(|id| hm.remove(id))
            .collect::<Vec<Option<Command>>>();

        Ok(commands)
    }

    #[graphql(arguments(
        limit(description = "optional paging limit, defaults to 100",),
        offset(description = "Offset into items, defaults to 0"),
        dir(description = "Sort direction, defaults to asc"),
        message(
            description = "Pattern to search for in message. Uses Postgres pattern matching  (https://www.postgresql.org/docs/9.6/functions-matching.html)"
        ),
        fqdn(
            description = "Pattern to search for in FQDN. Uses Postgres pattern matching  (https://www.postgresql.org/docs/9.6/functions-matching.html)"
        ),
        tag(
            description = "Pattern to search for in tag. Uses Postgres pattern matching  (https://www.postgresql.org/docs/9.6/functions-matching.html)"
        ),
        start_datetime(description = "Start of the time period of logs"),
        end_datetime(description = "End of the time period of logs"),
        message_class(description = "Array of log message classes"),
        severity(description = "Upper bound of log severity"),
    ))]
    /// Returns aggregated journal entries for all nodes the agent runs on.
    async fn logs(
        context: &Context,
        limit: Option<i32>,
        offset: Option<i32>,
        dir: Option<SortDir>,
        message: Option<String>,
        fqdn: Option<String>,
        tag: Option<String>,
        start_datetime: Option<chrono::DateTime<Utc>>,
        end_datetime: Option<chrono::DateTime<Utc>>,
        message_class: Option<Vec<MessageClass>>,
        severity: Option<LogSeverity>,
    ) -> juniper::FieldResult<LogResponse> {
        let dir = dir.unwrap_or_default();

        let message_class: Vec<_> = message_class
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![MessageClass::Normal])
            .into_iter()
            .map(|i| i as i16)
            .collect();

        let severity = severity.unwrap_or(LogSeverity::Informational) as i16;

        let results = sqlx::query_as!(
            LogMessageRecord,
            r#"
                    SELECT * FROM chroma_core_logmessage t
                    WHERE ($4::TEXT IS NULL OR t.message LIKE $4)
                      AND ($5::TEXT IS NULL OR t.fqdn LIKE $5)
                      AND ($6::TEXT IS NULL OR t.tag LIKE $6)
                      AND ($7::TIMESTAMPTZ IS NULL OR t.datetime >= $7)
                      AND ($8::TIMESTAMPTZ IS NULL OR t.datetime < $8)
                      AND ARRAY[t.message_class] <@ $9
                      AND t.severity <= $10
                    ORDER BY
                        CASE WHEN $3 = 'ASC' THEN t.datetime END ASC,
                        CASE WHEN $3 = 'DESC' THEN t.datetime END DESC
                    OFFSET $1 LIMIT $2"#,
            offset.unwrap_or(0) as i64,
            limit.map(|x| x as i64).unwrap_or(100),
            dir.deref(),
            message,
            fqdn,
            tag,
            start_datetime,
            end_datetime,
            &message_class,
            severity,
        )
        .fetch_all(&context.pg_pool)
        .await?;
        let xs: Vec<LogMessage> = results
            .into_iter()
            .map(|x| x.try_into())
            .collect::<Result<_, _>>()?;

        let total_count = sqlx::query!(
            "SELECT total_rows FROM rowcount WHERE table_name = 'chroma_core_logmessage';"
        )
        .fetch_one(&context.pg_pool)
        .await?
        .total_rows
        .ok_or_else(|| FieldError::new("Number of rows doesn't fit in i32", Value::null()))?;

        Ok(LogResponse {
            data: xs,
            meta: Meta {
                total_count: total_count.try_into()?,
            },
        })
    }

    async fn server_profiles(context: &Context) -> juniper::FieldResult<Vec<ServerProfile>> {
        let server_profile_records = sqlx::query!(
            r#"
                SELECT jsonb_agg((r.repo_name, r.location))
                    AS repos, sp.*
                    FROM chroma_core_repo AS r
                    INNER JOIN chroma_core_serverprofile_repolist AS rl ON r.repo_name = rl.repo_id
                    INNER JOIN chroma_core_serverprofile AS sp ON rl.serverprofile_id = sp.name
                    GROUP BY sp.name;
            "#,
        )
        .fetch_all(&context.pg_pool)
        .await?;

        let server_profiles: Vec<_> = server_profile_records
            .into_iter()
            .filter_map(|spr| {
                // TODO: Try to derive this somehow
                let record = ServerProfileRecord {
                    corosync: spr.corosync,
                    corosync2: spr.corosync2,
                    default: spr.default,
                    initial_state: spr.initial_state,
                    managed: spr.managed,
                    name: spr.name,
                    ntp: spr.ntp,
                    pacemaker: spr.pacemaker,
                    ui_description: spr.ui_description,
                    ui_name: spr.ui_name,
                    user_selectable: spr.user_selectable,
                    worker: spr.worker,
                };
                let repos = spr.repos?;
                ServerProfile::new(record, &repos).ok()
            })
            .collect();

        Ok(server_profiles)
    }
    /// List the client mount source.
    /// This will build up the source using known mgs locations
    /// for the given filesystem.
    #[graphql(arguments(fs(description = "The filesystem to generate the source for"),))]
    async fn client_mount_source(
        context: &Context,
        fs_name: String,
    ) -> juniper::FieldResult<String> {
        let x = client_mount_source(&context.pg_pool, &fs_name).await?;

        Ok(x)
    }
    /// List the full client mount command.
    /// This will build up the source using known mgs locations
    /// for the given filesystem.
    /// The output can be directly pasted on a server to mount a client,
    /// after the mount directory has been created.
    #[graphql(arguments(fs(description = "The filesystem to generate the mount command for"),))]
    async fn client_mount_command(
        context: &Context,
        fs_name: String,
    ) -> juniper::FieldResult<String> {
        let x = client_mount_source(&context.pg_pool, &fs_name).await?;

        let mount_command = format!("mount -t lustre {} /mnt/{}", x, fs_name);

        Ok(mount_command)
    }
}

pub(crate) struct MutationRoot;

#[juniper::graphql_object(Context = Context)]
impl MutationRoot {
    fn filesystem(&self) -> filesystem::FilesystemMutation {
        filesystem::FilesystemMutation
    }
    fn stratagem(&self) -> stratagem::StratagemMutation {
        stratagem::StratagemMutation
    }
    fn task(&self) -> task::TaskMutation {
        task::TaskMutation
    }
    fn snapshot(&self) -> snapshot::SnapshotMutation {
        snapshot::SnapshotMutation
    }

    /// Create a server profile.
    #[graphql(arguments(profile(description = "The server profile to add")))]
    async fn create_server_profile(
        context: &Context,
        profile: ServerProfileInput,
    ) -> juniper::FieldResult<bool> {
        let repolist = profile.repolist;
        let repolist_len = repolist.len();

        let count = sqlx::query!(
            "SELECT repo_name from chroma_core_repo where repo_name = ANY($1)",
            &repolist.clone()
        )
        .fetch_all(&context.pg_pool)
        .await?
        .len();

        if count != repolist_len {
            return Err(FieldError::new(
                format!("Repos not found for profile {}", profile.name),
                Value::null(),
            ));
        }

        let mut transaction = context.pg_pool.begin().await?;

        sqlx::query!(
            r#"
            INSERT INTO chroma_core_serverprofile
            (
                name,
                ui_name,
                ui_description,
                managed,
                worker,
                user_selectable,
                initial_state,
                ntp,
                corosync,
                corosync2,
                pacemaker,
                "default"
            )
            VALUES
            ($1, $2, $3, $4, $5, 'true', $6, $7, $8, $9, $10, 'false')
            ON CONFLICT (name)
            DO UPDATE
            SET
            ui_name = excluded.ui_name,
            ui_description = excluded.ui_description,
            managed = excluded.managed,
            worker = excluded.worker,
            user_selectable = excluded.user_selectable,
            initial_state = excluded.initial_state,
            ntp = excluded.ntp,
            corosync = excluded.corosync,
            corosync2 = excluded.corosync2,
            pacemaker = excluded.pacemaker,
            "default" = excluded.default
        "#,
            &profile.name,
            &profile.ui_name,
            &profile.ui_description,
            &profile.managed,
            &profile.worker,
            &profile.initial_state,
            &profile.ntp,
            &profile.corosync,
            &profile.corosync2,
            &profile.pacemaker
        )
        .execute(&mut transaction)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO chroma_core_serverprofile_repolist (serverprofile_id, repo_id)
            SELECT $1, repo_id
            FROM UNNEST($2::text[])
            AS t(repo_id)
            ON CONFLICT DO NOTHING
        "#,
            &profile.name,
            &repolist
        )
        .execute(&mut transaction)
        .await?;

        sqlx::query!(
            r#"
            INSERT INTO chroma_core_serverprofilepackage (package_name, server_profile_id)
            SELECT package_name, $2
            FROM UNNEST($1::text[])
            as t(package_name)
            ON CONFLICT DO NOTHING
            "#,
            &profile.packages.into_iter().collect::<Vec<_>>(),
            &profile.name
        )
        .execute(&mut transaction)
        .await?;

        transaction.commit().await?;
        Ok(true)
    }

    #[graphql(arguments(profile_name(description = "Name of the profile to remove")))]
    async fn remove_server_profile(
        context: &Context,
        profile_name: String,
    ) -> juniper::FieldResult<bool> {
        let mut transaction = context.pg_pool.begin().await?;

        sqlx::query!(
            "DELETE FROM chroma_core_serverprofile_repolist WHERE serverprofile_id = $1",
            &profile_name
        )
        .execute(&mut transaction)
        .await?;

        sqlx::query!(
            "DELETE FROM chroma_core_serverprofilepackage WHERE server_profile_id = $1",
            &profile_name
        )
        .execute(&mut transaction)
        .await?;

        sqlx::query!(
            "DELETE FROM chroma_core_serverprofile WHERE name = $1",
            &profile_name
        )
        .execute(&mut transaction)
        .await?;

        transaction.commit().await?;
        Ok(true)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct CommandTmpRecord {
    pub cancelled: bool,
    pub complete: bool,
    pub created_at: DateTime<Utc>,
    pub errored: bool,
    pub id: i32,
    pub job_ids: Option<Vec<i32>>,
    pub message: String,
}

fn to_command(x: CommandTmpRecord) -> Command {
    Command {
        id: x.id,
        cancelled: x.cancelled,
        complete: x.complete,
        errored: x.errored,
        created_at: x.created_at.format("%Y-%m-%dT%T%.6f").to_string(),
        jobs: {
            x.job_ids
                .unwrap_or_default()
                .into_iter()
                .map(|job_id: i32| format!("/api/{}/{}/", Job::<()>::endpoint_name(), job_id))
                .collect::<Vec<_>>()
        },
        logs: "".to_string(),
        message: x.message.clone(),
        resource_uri: format!("/api/{}/{}/", Command::endpoint_name(), x.id),
    }
}

pub(crate) type Schema = RootNode<'static, QueryRoot, MutationRoot, EmptySubscription<Context>>;

pub(crate) struct Context {
    pub(crate) pg_pool: PgPool,
    pub(crate) rabbit_pool: Pool,
}

impl juniper::Context for Context {}

pub(crate) async fn graphql(
    schema: Arc<Schema>,
    ctx: Arc<Context>,
    req: GraphQLRequest,
) -> Result<impl warp::Reply, warp::Rejection> {
    let res = req.execute(&schema, &ctx).await;
    let json = serde_json::to_string(&res).map_err(ImlApiError::SerdeJsonError)?;

    Ok(json)
}

pub(crate) fn endpoint(
    schema_filter: impl Filter<Extract = (Arc<Schema>,), Error = Infallible> + Clone + Send,
    ctx_filter: impl Filter<Extract = (Arc<Context>,), Error = Infallible> + Clone + Send,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let graphql_route = warp::path!("graphql")
        .and(warp::post())
        .and(schema_filter.clone())
        .and(ctx_filter)
        .and(warp::body::json())
        .and_then(graphql);

    let graphiql_route = warp::path!("graphiql")
        .and(warp::get())
        .map(|| warp::reply::html(graphiql_source("graphql", None)));

    let graphql_schema_route = warp::path!("graphql_schema")
        .and(warp::get())
        .and(schema_filter)
        .map(|schema: Arc<Schema>| schema.as_schema_language());

    graphql_route.or(graphiql_route).or(graphql_schema_route)
}

async fn get_fs_target_resources(
    pool: &PgPool,
    fs_name: Option<String>,
) -> Result<Vec<TargetResource>, ImlApiError> {
    let banned_resources = get_banned_targets(pool).await?;

    let xs = sqlx::query!(r#"
            SELECT
                rh.cluster_id,
                r.name as id,
                t.name,
                t.mount_path,
                t.filesystems,
                t.uuid,
                t.state,
                array_agg(DISTINCT rh.host_id) AS "cluster_hosts!"
            FROM target t
            INNER JOIN corosync_resource r ON r.mount_point = t.mount_path
            INNER JOIN corosync_resource_managed_host rh ON rh.corosync_resource_id = r.name AND rh.host_id = ANY(t.host_ids)
            WHERE CARDINALITY(t.filesystems) > 0
            GROUP BY rh.cluster_id, t.name, r.name, t.mount_path, t.uuid, t.filesystems, t.state
        "#)
            .fetch(pool)
            .try_filter(|x| {
                let x = match fs_name.as_ref() {
                    None => true,
                    Some(fs) => (&x.filesystems).contains(fs)
                };

                future::ready(x)
            })
            .map_ok(|mut x| {
                let xs:HashSet<_> = banned_resources
                    .iter()
                    .filter(|y| {
                        y.cluster_id == x.cluster_id && y.resource == x.id &&  x.mount_path == y.mount_point
                    })
                    .map(|y| y.host_id)
                    .collect();

                x.cluster_hosts.retain(|id| !xs.contains(id));

                x
            })
            .map_ok(|x| {
                TargetResource {
                    cluster_id: x.cluster_id,
                    fs_names: x.filesystems,
                    uuid: x.uuid,
                    name: x.name,
                    resource_id: x.id,
                    state: x.state,
                    cluster_hosts: x.cluster_hosts
                }
            }).try_collect()
            .await?;

    Ok(xs)
}

async fn get_fs_cluster_hosts(
    pool: &PgPool,
    fs_name: String,
) -> Result<Vec<Vec<String>>, ImlApiError> {
    let xs = get_fs_target_resources(pool, Some(fs_name))
        .await?
        .into_iter()
        .group_by(|x| x.cluster_id);

    let xs: Vec<Vec<i32>> = xs.into_iter().fold(vec![], |mut acc, (_, xs)| {
        let xs: HashSet<i32> = xs.map(|x| x.cluster_hosts).flatten().collect();

        acc.push(xs.into_iter().collect());

        acc
    });
    let xs = xs.into_iter().map(|idset| async move {
        let fqdns = idset
            .into_iter()
            .map(|x| async move { fqdn_by_host_id(pool, x).await });
        join_all(fqdns)
            .await
            .into_iter()
            .filter_map(|x| x.ok())
            .collect()
    });

    Ok(join_all(xs).await)
}

async fn get_banned_targets(pool: &PgPool) -> Result<Vec<BannedTargetResource>, ImlApiError> {
    let xs = sqlx::query!(
        r#"
            SELECT b.id, b.resource, b.node, b.cluster_id, nh.host_id, t.mount_point
            FROM corosync_resource_bans b
            INNER JOIN corosync_node_managed_host nh ON (nh.corosync_node_id).name = b.node
            AND nh.cluster_id = b.cluster_id
            INNER JOIN corosync_resource t ON t.name = b.resource AND b.cluster_id = t.cluster_id
            WHERE t.mount_point is not NULL
        "#
    )
    .fetch(pool)
    .map_ok(|x| BannedTargetResource {
        resource: x.resource,
        cluster_id: x.cluster_id,
        host_id: x.host_id,
        mount_point: x.mount_point,
    })
    .try_collect()
    .await?;

    Ok(xs)
}

async fn get_banned_resources(pool: &PgPool) -> Result<Vec<BannedResource>, ImlApiError> {
    let xs = sqlx::query_as!(
        BannedResource,
        r#"
            SELECT * FROM corosync_resource_bans
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(xs)
}

async fn fs_id_by_name(pool: &PgPool, name: &str) -> Result<i32, juniper::FieldError> {
    sqlx::query!(
        "SELECT id FROM chroma_core_managedfilesystem WHERE name=$1 and not_deleted = 't'",
        name
    )
    .fetch_optional(pool)
    .await?
    .map(|x| x.id)
    .ok_or_else(|| FieldError::new(format!("Filesystem {} not found", name), Value::null()))
}

async fn insert_task(
    name: &str,
    state: &str,
    single_runner: bool,
    keep_failed: bool,
    actions: &[String],
    args: serde_json::Value,
    fs_id: i32,
    pool: &PgPool,
) -> Result<Task, ImlApiError> {
    let x = sqlx::query_as!(
        Task,
        r#"
                INSERT INTO chroma_core_task (
                    name,
                    start,
                    state,
                    fids_total,
                    fids_completed,
                    fids_failed,
                    data_transfered,
                    single_runner,
                    keep_failed,
                    actions,
                    args,
                    filesystem_id
                )
                VALUES (
                    $1,
                    now(),
                    $2,
                    0,
                    0,
                    0,
                    0,
                    $3,
                    $4,
                    $5,
                    $6,
                    $7
                )
                RETURNING *
            "#,
        name,
        state,
        single_runner,
        keep_failed,
        actions,
        args,
        fs_id
    )
    .fetch_one(pool)
    .await?;

    Ok(x)
}

async fn insert_fidlist(fids: Vec<String>, task_id: i32, pool: &PgPool) -> Result<(), ImlApiError> {
    let x = fids
        .iter()
        .map(|fid| LustreFid::from_str(&fid))
        .filter_map(Result::ok)
        .fold((vec![], vec![], vec![]), |mut acc, fid| {
            acc.0.push(fid.seq);
            acc.1.push(fid.oid);
            acc.2.push(fid.ver);

            acc
        });

    sqlx::query!(
        r#"
	    INSERT INTO chroma_core_fidtaskqueue (fid, data, task_id)
            SELECT row(seq, oid, ver)::lustre_fid, '{}'::jsonb, $4
            FROM UNNEST($1::bigint[], $2::int[], $3::int[])
            AS t(seq, oid, ver)"#,
        &x.0,
        &x.1,
        &x.2,
        task_id
    )
    .execute(pool)
    .await?;

    // update the number of fids in the task
    sqlx::query!(
        r#"
        UPDATE chroma_core_task
        SET fids_total = fids_total + $1
        WHERE id = $2
    "#,
        fids.len() as i64,
        task_id
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn run_jobs<T: std::fmt::Debug + serde::Serialize>(
    msg: impl ToString,
    jobs: Vec<SendJob<'_, T>>,
    rabbit_pool: &Pool,
) -> Result<i32, ImlApiError> {
    let kwargs: HashMap<String, String> = vec![("message".into(), msg.to_string())]
        .into_iter()
        .collect();

    let id: i32 = iml_job_scheduler_rpc::call(
        &rabbit_pool.get().await.map_err(ImlRabbitError::PoolError)?,
        "run_jobs",
        vec![jobs],
        Some(kwargs),
    )
    .map_err(ImlApiError::ImlJobSchedulerRpcError)
    .await?;

    Ok(id)
}

fn create_task_job<'a>(task_id: i32) -> SendJob<'a, HashMap<String, serde_json::Value>> {
    SendJob {
        class_name: "CreateTaskJob",
        args: vec![("task_id".into(), serde_json::json!(task_id))]
            .into_iter()
            .collect(),
    }
}

async fn client_mount_source(pg_pool: &PgPool, fs_name: &str) -> Result<String, ImlApiError> {
    let nids = sqlx::query!(
        r#"
            SELECT n.nid FROM target AS t
            INNER JOIN lnet as l ON l.host_id = ANY(t.host_ids)
            INNER JOIN nid as n ON n.id = ANY(l.nids)
            WHERE t.name='MGS' AND $1 = ANY(t.filesystems)
            AND n.host_id NOT IN (
                SELECT nh.host_id
                FROM corosync_resource_bans b
                INNER JOIN corosync_node_managed_host nh ON (nh.corosync_node_id).name = b.node
                AND nh.cluster_id = b.cluster_id
                INNER JOIN corosync_resource r ON r.name = b.resource AND b.cluster_id = r.cluster_id
                WHERE r.mount_point is not NULL AND r.mount_point = t.mount_path
            )
            GROUP BY l.host_id, n.nid ORDER BY l.host_id, n.nid
            "#,
        fs_name
    )
    .fetch_all(pg_pool)
    .await?
    .into_iter()
    .map(|x| x.nid)
    .collect::<Vec<String>>();

    let mount_command = format!("{}:/{}", nids.join(":"), fs_name);

    Ok(mount_command)
}
