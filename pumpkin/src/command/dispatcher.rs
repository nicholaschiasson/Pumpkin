use pumpkin_core::text::TextComponent;

use super::args::ConsumedArgs;

use crate::command::dispatcher::CommandError::{
    GeneralCommandIssue, InvalidConsumption, InvalidRequirement, OtherPumpkin,
};
use crate::command::tree::{Command, CommandTree, NodeType, RawArgs};
use crate::command::CommandSender;
use crate::error::PumpkinError;
use crate::server::Server;
use pumpkin_core::text::color::{Color, NamedColor};
use std::collections::HashMap;

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum CommandError {
    /// This error means that there was an error while parsing a previously consumed argument.
    /// That only happens when consumption is wrongly implemented, as it should ensure parsing may
    /// never fail.
    InvalidConsumption(Option<String>),

    /// Return this if a condition that a [`Node::Require`] should ensure is met is not met.
    InvalidRequirement,

    OtherPumpkin(Box<dyn PumpkinError>),

    GeneralCommandIssue(String),
}

impl CommandError {
    pub fn into_string_or_pumpkin_error(self, cmd: &str) -> Result<String, Box<dyn PumpkinError>> {
        match self {
            InvalidConsumption(s) => {
                println!("Error while parsing command \"{cmd}\": {s:?} was consumed, but couldn't be parsed");
                Ok("Internal Error (See logs for details)".into())
            }
            InvalidRequirement => {
                println!("Error while parsing command \"{cmd}\": a requirement that was expected was not met.");
                Ok("Internal Error (See logs for details)".into())
            }
            GeneralCommandIssue(s) => Ok(s),
            OtherPumpkin(e) => Err(e),
        }
    }
}

#[derive(Default)]
pub struct CommandDispatcher<'a> {
    pub(crate) commands: HashMap<&'a str, Command<'a>>,
}

/// Stores registered [`CommandTree`]s and dispatches commands to them.
impl<'a> CommandDispatcher<'a> {
    pub async fn handle_command(
        &'a self,
        sender: &mut CommandSender<'a>,
        server: &'a Server,
        cmd: &'a str,
    ) {
        if let Err(e) = self.dispatch(sender, server, cmd).await {
            match e.into_string_or_pumpkin_error(cmd) {
                Ok(err) => {
                    sender
                        .send_message(
                            TextComponent::text_string(err)
                                .color_named(pumpkin_core::text::color::NamedColor::Red),
                        )
                        .await;
                }
                Err(pumpkin_error) => {
                    pumpkin_error.log();
                    sender.send_message(TextComponent::text("Unknown internal error occured while running command. Please see server log").color(Color::Named(NamedColor::Red))).await;
                }
            }
        }
    }

    /// Execute a command using its corresponding [`CommandTree`].
    pub(crate) async fn dispatch(
        &'a self,
        src: &mut CommandSender<'a>,
        server: &'a Server,
        cmd: &'a str,
    ) -> Result<(), CommandError> {
        // Other languages dont use the ascii whitespace
        let mut parts = cmd.split_whitespace();
        let key = parts
            .next()
            .ok_or(GeneralCommandIssue("Empty Command".to_string()))?;
        let raw_args: Vec<&str> = parts.rev().collect();

        let tree = self.get_tree(key)?;

        // try paths until fitting path is found
        for path in tree.iter_paths() {
            if Self::try_is_fitting_path(src, server, &path, tree, raw_args.clone()).await? {
                return Ok(());
            }
        }
        Err(GeneralCommandIssue(format!(
            "Invalid Syntax. Usage: {tree}"
        )))
    }

    pub(crate) fn get_tree(&'a self, key: &str) -> Result<&'a CommandTree<'a>, CommandError> {
        let command = self
            .commands
            .get(key)
            .ok_or(GeneralCommandIssue("Command not found".to_string()))?;

        match command {
            Command::Tree(tree) => Ok(tree),
            Command::Alias(target) => {
                let Some(Command::Tree(tree)) = &self.commands.get(target) else {
                    println!("Error while parsing command alias \"{key}\": pointing to \"{target}\" which is not a valid tree");
                    return Err(GeneralCommandIssue(
                        "Internal Error (See logs for details)".into(),
                    ));
                };
                Ok(tree)
            }
        }
    }

    async fn try_is_fitting_path(
        src: &mut CommandSender<'a>,
        server: &'a Server,
        path: &[usize],
        tree: &CommandTree<'a>,
        mut raw_args: RawArgs<'a>,
    ) -> Result<bool, CommandError> {
        let mut parsed_args: ConsumedArgs = HashMap::new();

        for node in path.iter().map(|&i| &tree.nodes[i]) {
            match node.node_type {
                NodeType::ExecuteLeaf { executor } => {
                    return if raw_args.is_empty() {
                        executor.execute(src, server, &parsed_args).await?;
                        Ok(true)
                    } else {
                        Ok(false)
                    };
                }
                NodeType::Literal { string, .. } => {
                    if raw_args.pop() != Some(string) {
                        return Ok(false);
                    }
                }
                NodeType::Argument { consumer, name, .. } => {
                    match consumer.consume(src, server, &mut raw_args).await {
                        Some(consumed) => {
                            parsed_args.insert(name, consumed);
                        }
                        None => return Ok(false),
                    }
                }
                NodeType::Require { predicate, .. } => {
                    if !predicate(src) {
                        return Ok(false);
                    }
                }
            }
        }

        Ok(false)
    }

    /// Register a command with the dispatcher.
    pub(crate) fn register(&mut self, tree: CommandTree<'a>) {
        let mut names = tree.names.iter();

        let primary_name = names.next().expect("at least one name must be provided");

        for &name in names {
            self.commands.insert(name, Command::Alias(primary_name));
        }

        self.commands.insert(primary_name, Command::Tree(tree));
    }
}