//! What the IDE serves down an environment channel.
//!
//! `taste-devcontainer` opens the channel and demultiplexes it, but it
//! deliberately knows nothing about what rides on it — it depends on
//! neither the MCP server nor the auth proxy, and should not. This is the
//! one crate that can see both, so this is where the two ends are tied
//! together, in the smallest amount of code that can do it.
//!
//! The whole of the routing is: a connection tagged `Mcp` goes to this
//! workspace's MCP server as the environment the channel belongs to, and a
//! connection tagged `Auth` goes to the workspace's auth proxy. There is no
//! third case and no default — [`Service`] is a closed set, so a container
//! can ask for one of two things and nothing else.

use std::sync::Arc;

use taste_core::environment::EnvironmentId;
use taste_devcontainer::channel::{ChannelServices, ChannelStream, Service};
use taste_mcp::McpServer;

pub struct IdeChannelServices {
    mcp: Arc<McpServer>,
}

impl IdeChannelServices {
    pub fn new(mcp: Arc<McpServer>) -> Arc<Self> {
        Arc::new(Self { mcp })
    }
}

impl ChannelServices for IdeChannelServices {
    /// The MCP server is always there. The auth proxy may not be — it is
    /// off with `TASTE_AUTH_PROXY=0`, and it may have failed to bind when
    /// the workspace opened it — and saying so is what stops the hosting
    /// probe from failing an environment over a door the IDE never opened.
    /// This is a read either way: the proxy is started once, from the
    /// runtime, by `open_workspace`.
    fn serves(&self, service: Service) -> bool {
        match service {
            Service::Mcp => true,
            Service::Auth => taste_acp::authproxy::handle().is_some(),
        }
    }

    fn accept(&self, env: &EnvironmentId, service: Service, stream: ChannelStream) {
        match service {
            // The environment comes from the channel, never from the
            // caller: this is the same unforgeable identity the per-
            // environment socket gave, established the same way — by which
            // container the IDE exec'd into.
            Service::Mcp => self.mcp.clone().serve_stream(env.clone(), stream),
            Service::Auth => {
                if let Some(handle) = taste_acp::authproxy::handle() {
                    handle.serve_stream(stream);
                }
            }
        }
    }
}
