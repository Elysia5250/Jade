//! Slash command definitions for the TUI command menu.

pub(crate) struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub args: &'static str,
    pub about: &'static str,
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "login",
        aliases: &["connect"],
        args: "",
        about: "configure API Key or OAuth subscription",
    },
    SlashCommand {
        name: "models",
        aliases: &["model"],
        args: "[id]",
        about: "switch active LLM model",
    },
    SlashCommand {
        name: "logout",
        aliases: &[],
        args: "[provider]",
        about: "remove a saved provider credential",
    },
    SlashCommand {
        name: "compact",
        aliases: &[],
        args: "[focus]",
        about: "compact older conversation into a handoff summary",
    },
    SlashCommand {
        name: "handoff",
        aliases: &[],
        args: "[focus]",
        about: "summarize into a clean new session (context reset)",
    },
    SlashCommand {
        name: "budget",
        aliases: &[],
        args: "[tier|size]",
        about: "inspect or set compaction tier (native/focused/balanced/max/tokens)",
    },
    SlashCommand {
        name: "mode",
        aliases: &[],
        args: "",
        about: "switch between Agent and Shell mode",
    },
    SlashCommand {
        name: "permissions",
        aliases: &["perm"],
        args: "[mode|allow <rule>|deny <rule>]",
        about: "inspect or set permission mode and rules",
    },
    SlashCommand {
        name: "plan",
        aliases: &[],
        args: "",
        about: "switch to read-only Plan permission mode",
    },
    SlashCommand {
        name: "trust",
        aliases: &[],
        args: "[on|off]",
        about: "trust or untrust workspace project config",
    },
    SlashCommand {
        name: "mcp",
        aliases: &[],
        args: "[probe]",
        about: "inspect configured MCP servers or probe live tools",
    },
    SlashCommand {
        name: "skill",
        aliases: &["skills"],
        args: "[name]",
        about: "list or inspect discovered Agent Skills",
    },
    SlashCommand {
        name: "cd",
        aliases: &[],
        args: "<dir>",
        about: "change working directory (with path completion)",
    },
    SlashCommand {
        name: "machine",
        aliases: &["machines"],
        args: "[name]",
        about: "switch target machine (local or conduit peer)",
    },
    SlashCommand {
        name: "new",
        aliases: &[],
        args: "",
        about: "start a new session",
    },
    SlashCommand {
        name: "sessions",
        aliases: &["resume"],
        args: "",
        about: "list or switch saved sessions",
    },
    SlashCommand {
        name: "expand",
        aliases: &[],
        args: "",
        about: "expand or collapse tool output cards",
    },
    SlashCommand {
        name: "sidebar",
        aliases: &["panel"],
        args: "[reset]",
        about: "toggle right inspector panel (or reset width)",
    },
    SlashCommand {
        name: "diff",
        aliases: &[],
        args: "[file]",
        about: "open file diff review in the right inspector",
    },
    SlashCommand {
        name: "open",
        aliases: &["view"],
        args: "<path[:line]>",
        about: "open file in the right inspector",
    },
    SlashCommand {
        name: "mouse",
        aliases: &[],
        args: "",
        about: "toggle mouse capture (links/scroll vs native terminal selection)",
    },
    SlashCommand {
        name: "cancel",
        aliases: &[],
        args: "",
        about: "cancel active turn or selected job",
    },
    SlashCommand {
        name: "clear",
        aliases: &[],
        args: "",
        about: "clear current transcript",
    },
    SlashCommand {
        name: "theme",
        aliases: &["themes"],
        args: "",
        about: "switch colour theme",
    },
    SlashCommand {
        name: "help",
        aliases: &[],
        args: "",
        about: "show key bindings & commands",
    },
    SlashCommand {
        name: "quit",
        aliases: &["exit", "q"],
        args: "",
        about: "exit envoy",
    },
];
