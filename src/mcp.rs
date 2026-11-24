//! MCP stdio adapter for grafts.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};

use crate::api::{self, AddDep, DropWorkspace, Exec, Land, Name, Owned, TurnRelease, TurnTake};
use crate::workspace::{self, Command, Spawn};

const MAX_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Clone)]
struct Server {
    repo: PathBuf,
    allow_exec: bool,
}

#[derive(serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

fn tool<T: JsonSchema>(
    name: &'static str,
    description: &'static str,
    read_only: bool,
    destructive: bool,
) -> Tool {
    let schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    let schema = schema
        .as_object()
        .expect("tool schema is an object")
        .clone();
    Tool::new(name, description, schema).with_annotations(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(destructive),
    )
}

fn parse<T: DeserializeOwned>(args: Map<String, Value>) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(args)).map_err(invalid)
}

fn invalid(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("Invalid tool arguments: {error}"), None)
}

impl Server {
    /// Run one operation off the async thread.
    async fn run(&self, op: Command) -> Result<anyhow::Result<Value>, ErrorData> {
        let repo = self.repo.clone();
        tokio::task::spawn_blocking(move || workspace::run(&repo, op))
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Graft operation failed to join: {e}"), None)
            })
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let instructions = if self.allow_exec {
            "Running commands and installing dependencies in grafts is enabled by the host."
        } else {
            "Running commands and installing dependencies in grafts is disabled; the host must start with --allow-exec to enable it."
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "{instructions} A graft is an independent copy of the repository for one task. Call graft_add, work inside the returned path, then graft_merge to merge it into a branch. Before using something every graft shares, such as a development server on a fixed port, call graft_turn_take, and graft_turn_release when done. A graft holds only tracked files: untracked and ignored files such as .env, local config or build output stay behind unless you copy them. Decide what the task needs: call graft_local_files to see them, then pass include to graft_add with .gitignore-style patterns for just the ones to bring. Prefer naming them: [\"*\"] copies all of them, including build output and directories such as target/ and .venv/, which can be large. Omitting include uses the project's .worktreeinclude file if it has one, and copies nothing otherwise. For ordinary work in a trusted clone, omit history from graft_add to honor the project setting (default: local). Choose isolated when the graft should only receive the selected commit's reachable history. Each newline-delimited message may contain up to {MAX_REQUEST_BYTES} bytes. Larger messages are discarded."
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![
            tool::<Spawn>(
                "graft_add",
                "Create a graft: an independent copy of the repository with its own branch. Returns the graft's path and lease; keep the lease, it authorizes every later call. Omit history for normal work (the project setting defaults to local, which reuses the trusted clone's Git objects). Choose isolated to keep unrelated objects such as staged blobs or other branches' commits out. Untracked and ignored files, such as .env, are not copied unless include names them (see graft_local_files) or the project has a .worktreeinclude file. Name what the task needs; [\"*\"] copies all of them, including large build and environment directories.",
                false,
                false,
            ),
            tool::<Empty>(
                "graft_list",
                "List active and recoverable grafts.",
                true,
                false,
            ),
            tool::<Empty>(
                "graft_local_files",
                "List the checkout's untracked and ignored files and directories, such as .env, local config and build output. A new graft holds none of them; pass the ones its task needs to graft_add as include patterns. Also says whether the project's .worktreeinclude file sets a default.",
                true,
                false,
            ),
            tool::<Name>(
                "graft_status",
                "Show a graft's HEAD, branch and changed files.",
                true,
                false,
            ),
        ];
        if self.allow_exec {
            tools.push(tool::<Exec>(
                "graft_exec",
                "Run a program inside a graft. A registered package manager matches the program name automatically; pass manager for wrappers. Needs host --allow-exec.",
                false,
                true,
            ));
            tools.push(tool::<AddDep>(
                "graft_add_dependency",
                "Install packages in a graft, or run a locked install when packages is empty. Needs host --allow-exec.",
                false,
                true,
            ));
        }
        tools.extend([
            tool::<Land>(
                "graft_merge",
                "Merge a graft's commits into a local branch, then archive the graft. Set undo to reverse an earlier merge instead: it appends a commit only if the branch is still at the merged commit. Lockfile reconciliation needs host --allow-exec.",
                false,
                true,
            ),
            tool::<DropWorkspace>(
                "graft_remove",
                "Archive a graft and its files. Unless discard is set, refuses and names what it found when the graft has uncommitted, untracked or ignored files, unmerged commits, a stash, or extra branches or tags; keep the recovery lease to reopen it.",
                false,
                true,
            ),
            tool::<Owned>(
                "graft_restore",
                "Reopen an archived graft using its recovery lease.",
                false,
                false,
            ),
            tool::<Owned>(
                "graft_renew",
                "Renew a graft's lease so it is not treated as expired.",
                false,
                false,
            ),
            tool::<TurnTake>(
                "graft_turn_take",
                "Take a turn on something the grafts share and cannot each have a copy of, such as a development server on a fixed port or a local database. resource is a name the project chooses. granted true means this graft holds the turn. Otherwise it is queued and position is its place in line, 1 being next: call again within 300 seconds to keep the place, or set wait to keep asking, for less time than your client allows a tool call. Call again while you hold the turn to renew it before expires_at. Graft does nothing to the resource: a turn is a promise between cooperating grafts. Give it up with graft_turn_release as soon as you finish.",
                false,
                false,
            ),
            tool::<TurnRelease>(
                "graft_turn_release",
                "Give up this graft's turn on a shared resource, or leave its queue. next names the graft now first in line; it gets the turn when it next calls graft_turn_take.",
                false,
                false,
            ),
            tool::<Empty>(
                "graft_turn_list",
                "List the shared resources that have a holder or a queue: who holds each turn, when it expires and who waits, in order.",
                true,
                false,
            ),
        ]);
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let refuse = |message: String| -> Result<CallToolResponse, ErrorData> {
            let error = anyhow::anyhow!(message);
            Ok(CallToolResult::structured_error(api::error_json(&error)).into())
        };
        if !self.allow_exec && matches!(name, "graft_exec" | "graft_add_dependency") {
            return refuse(format!(
                "Tool {name} requires the host to start with --allow-exec"
            ));
        }
        let args = request.arguments.unwrap_or_default();
        let op = match name {
            "graft_list" => parse::<Empty>(args).map(|_| Command::List)?,
            "graft_local_files" => parse::<Empty>(args).map(|_| Command::LocalFiles)?,
            "graft_add" => {
                Command::Spawn(Spawn::from_mcp(args, !self.allow_exec).map_err(invalid)?)
            }
            "graft_status" => Command::Status(parse(args)?),
            "graft_renew" => Command::Heartbeat(parse(args)?),
            "graft_exec" => Command::Exec(parse(args)?),
            "graft_add_dependency" => Command::AddDep(parse(args)?),
            "graft_restore" => Command::Restore(parse(args)?),
            "graft_merge" => {
                let explicit_into = args.contains_key("into");
                let land: Land = parse(args)?;
                land.check_undo(explicit_into).map_err(invalid)?;
                if land.reconcile && !self.allow_exec {
                    return refuse(
                        "Lockfile reconciliation requires the host to start with --allow-exec"
                            .into(),
                    );
                }
                Command::land(land)
            }
            "graft_remove" => Command::Drop(parse(args)?),
            "graft_turn_take" => {
                let take: TurnTake = parse(args)?;
                take.check().map_err(invalid)?;
                Command::TurnTake(take)
            }
            "graft_turn_release" => Command::TurnRelease(parse(args)?),
            "graft_turn_list" => parse::<Empty>(args).map(|_| Command::TurnList)?,
            _ => {
                return Err(ErrorData::invalid_params(
                    format!("Unknown tool: {name}"),
                    None,
                ));
            }
        };
        let result = match op {
            // A waiting take asks from here, one whole operation at a time,
            // so it stops asking when the client cancels the request or
            // disconnects instead of taking a turn nobody will use.
            Command::TurnTake(mut take) => {
                let deadline = Instant::now() + Duration::from_secs(take.wait);
                take.wait = 0;
                loop {
                    let reply = self.run(Command::TurnTake(take.clone())).await?;
                    let pause = reply
                        .as_ref()
                        .ok()
                        .and_then(|reply| workspace::turn_pause(reply, deadline));
                    let Some(pause) = pause else { break reply };
                    let cancelled = tokio::time::timeout(pause, context.ct.cancelled());
                    if cancelled.await.is_ok() {
                        break reply;
                    }
                }
            }
            op => self.run(op).await?,
        };
        Ok(match result {
            Ok(value) if value.get("success") == Some(&Value::Bool(false)) => {
                CallToolResult::structured_error(value)
            }
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(api::error_json(&error)),
        }
        .into())
    }
}

/// Bound rmcp's stdin framing buffer before it parses or dispatches requests.
fn bounded_input(input: impl AsyncRead + Send + Unpin + 'static) -> impl AsyncRead + Send + Unpin {
    let (mut writer, reader) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut input = BufReader::new(input);
        let mut frame = Vec::new();
        let mut oversized = false;
        loop {
            let chunk = input.fill_buf().await?;
            if chunk.is_empty() {
                return Ok::<_, std::io::Error>(());
            }
            let newline = chunk.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(chunk.len(), |index| index + 1);
            oversized |= frame.len() + consumed > MAX_REQUEST_BYTES + 1;
            if !oversized {
                frame.extend_from_slice(&chunk[..consumed]);
            }
            input.consume(consumed);
            if newline.is_some() {
                if !oversized {
                    writer.write_all(&frame).await?;
                }
                frame.clear();
                oversized = false;
            }
        }
    });
    reader
}

/// Serve MCP over stdin/stdout until the client closes the connection.
pub fn serve(repo: PathBuf, allow_exec: bool) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let (stdin, stdout) = rmcp::transport::stdio();
            Server { repo, allow_exec }
                .serve((bounded_input(stdin), stdout))
                .await?
                .waiting()
                .await?;
            Ok(())
        })
}
