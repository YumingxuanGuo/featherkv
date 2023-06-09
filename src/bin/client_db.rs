use featherdb::concurrency::Mode;
use featherdb::error::{Error, Result};
use featherdb::proto::featherdb::{FeatherDbClient, ExecutionArgs, RegistrationArgs, ExecutionReply};
use featherdb::sql::execution::ResultSet;
use serde::Deserialize;
use rustyline::error::ReadlineError;
use rustyline::{Editor, history::DefaultHistory};
use rustyline::validate::{Validator, ValidationContext, ValidationResult};
use rustyline_derive::{Completer, Helper, Highlighter, Hinter};
use tonic::transport::Channel;

use featherdb::server::{ClientRequest, serialize, deserialize, ClientResponse};
use featherdb::sql::parser::{Lexer, Token, Symbol};

#[tokio::main]
async fn main() -> Result<()> {
    let mut client = FeatherClient::new().await?;
    client.run().await
}

/// The FeatherClient that provides a REPL interface.
struct FeatherClient {
    editor: Editor<InputValidator, DefaultHistory>,
    show_headers: bool,
    client: FeatherDbClient<Channel>,
    session_id: u64,
    sequence_number: u64,
}

impl FeatherClient {
    /// Creates a new ToySQL REPL for the given server host and port.
    async fn new() -> Result<Self> {
        let config = Config::new("config/client_db.yaml")?;
        let addr = format!("http://{}", config.db_addr);
        let mut client = match FeatherDbClient::connect(addr).await {
            Ok(client) => client,
            Err(err) => return Err(Error::Internal(err.to_string())),
        };
        let session_id = client.register(RegistrationArgs { }).await?.into_inner().session_id;

        let editor = Editor::new()?;
        Ok(Self { editor, show_headers: false, client, session_id, sequence_number: 1 })
    }

    /// Runs the REPL.
    async fn run(&mut self) -> Result<()> {
        self.editor.set_helper(Some(InputValidator));
        // Make sure multiline pastes are interpreted as normal inputs.
        self.editor.bind_sequence(
            rustyline::KeyEvent(rustyline::KeyCode::BracketedPasteStart, rustyline::Modifiers::NONE),
            rustyline::Cmd::Noop,
        );
        println!("Connected to featherDB. Enter !help for instructions.");

        while let Some(input) = self.prompt()? {
            match self.execute(&input).await {
                Ok(_) => {},
                err @ Err(Error::Internal(_)) => return err,
                Err(err) => {
                    let msg = err.to_string();
                    let chunks = msg.split(" ").collect::<Vec<_>>();
                    println!("  Error: {}", chunks[1..].join(" "));
                }
            }
            self.sequence_number += 1;
        }

        Ok(())
    }

    /// Prompts the user for input.
    fn prompt(&mut self) -> Result<Option<String>> {
        // TODO: different prompts for transactions
        let prompt = "featherDB> ".to_string();
        match self.editor.readline(&prompt) {
            Ok(input) => {
                self.editor.add_history_entry(&input)?;
                Ok(Some(input.trim().to_string()))
            }
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Executes a request. TODO: Retry on failure.
    async fn execute(&mut self, input: &str) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        match input.starts_with('!') {
            true => self.execute_command(input).await,
            false => self.execute_query(input).await,
        }
    }

    /// Runs a query and displays the results.
    async fn execute_query(&mut self, query: &str) -> Result<()> {
        let request = tonic::Request::new(ExecutionArgs {
            session_id: self.session_id,
            sequence_number: self.sequence_number,
            client_request: serialize(&ClientRequest::Query(query.to_string()))?,
        });
        let ExecutionReply { result } = self.client.execute(request).await?.into_inner();

        match deserialize::<Result<ClientResponse>>(&result)?? {
            ClientResponse::Query(result_set) => {
                match result_set {
                    ResultSet::Begin { id, mode } => match mode {
                        Mode::ReadWrite => println!("  Began transaction {}", id),
                        Mode::ReadOnly => println!("  Began read-only transaction {}", id),
                        Mode::Snapshot { version, .. } => println!(
                            "  Began read-only transaction {} in snapshot at version {}",
                            id, version
                        ),
                    },
                    ResultSet::Commit { id } => println!("  Committed transaction {}", id),
                    ResultSet::Rollback { id } => println!("  Rolled back transaction {}", id),
                    ResultSet::Create { count } => println!("  Created {} rows", count),
                    ResultSet::Delete { count } => println!("  Deleted {} rows", count),
                    ResultSet::Update { count } => println!("  Updated {} rows", count),
                    ResultSet::CreateTable { name } => println!("  Created table {}", name),
                    ResultSet::DropTable { name } => println!("  Dropped table {}", name),
                    ResultSet::Explain(plan) => println!("{}", plan.to_string()),
                    ResultSet::Query { columns, buffered_rows, .. } => {
                        if self.show_headers {
                            println!(
                                "  {}",
                                columns
                                    .iter()
                                    .map(|c| c.name.as_deref().unwrap_or("?"))
                                    .collect::<Vec<_>>()
                                    .join("|")
                            );
                        }
                        let mut iter = buffered_rows?.into_iter();
                        while let Some(row) = iter.next() {
                            println!(
                                "  {}",
                                row.into_iter().map(|v| format!("{}", v)).collect::<Vec<_>>().join("|")
                            );
                        }
                    },
                }
            },
            _ => return Err(Error::Internal("  Unexpected reply.".to_string())),
        }

        Ok(())
    }

    /// Handles a REPL command (prefixed by !, e.g. !help)
    async fn execute_command(&mut self, input: &str) -> Result<()> {
        let mut input = input.split_ascii_whitespace();
        let command = input.next().ok_or_else(|| Error::Parse("Expected command.".to_string()))?;

        let getargs = |n| {
            let args: Vec<&str> = input.collect();
            if args.len() != n {
                Err(Error::Parse(format!("{}: expected {} args, got {}", command, n, args.len())))
            } else {
                Ok(args)
            }
        };
        
        match command {
            "!headers" => match getargs(1)?[0] {
                "on" => {
                    self.show_headers = true;
                    println!("  Headers enabled");
                }
                "off" => {
                    self.show_headers = false;
                    println!("  Headers disabled");
                }
                v => return Err(Error::Parse(format!("Invalid value {}, expected on or off", v))),
            },

            "!help" => println!(
                r#"
Enter a SQL statement terminated by a semicolon (;) to execute it and display the result.
The following commands are also available:

    !headers <on|off>  Enable or disable column headers
    !help              This help message
    !status            Display server status
    !table [table]     Display table schema, if it exists
    !tables            List tables
"#
            ),

            "!status" => {
                todo!()
            },

            "!table" => {
                let request = tonic::Request::new(ExecutionArgs {
                    session_id: self.session_id,
                    sequence_number: self.sequence_number,
                    client_request: serialize(&ClientRequest::GetTable(getargs(1)?[0].to_string()))?,
                });
                let reply = self.client.execute(request).await?.into_inner();

                match deserialize::<Result<ClientResponse>>(&reply.result)?? {
                    ClientResponse::GetTable(table) => println!("{}", table),
                    _ => return Err(Error::Internal("Unexpected reply.".to_string())),
                }
            }

            "!tables" => {
                getargs(0)?;
                let request = tonic::Request::new(ExecutionArgs {
                    session_id: self.session_id,
                    sequence_number: self.sequence_number,
                    client_request: serialize(&ClientRequest::ListTables)?,
                });
                let reply = self.client.execute(request).await?.into_inner();
                
                match deserialize::<Result<ClientResponse>>(&reply.result)?? {
                    ClientResponse::ListTables(tables) => {
                        for table in tables {
                            println!("{}", table);
                        }
                    },
                    _ => return Err(Error::Internal("Unexpected reply.".to_string())),
                }
            }

            c => return Err(Error::Parse(format!("Unknown command {}", c))),
        }

        Ok(())
    }
}

/// A Rustyline helper for multiline editing. It parses input lines and determines if they make up a
/// complete command or not.
#[derive(Completer, Helper, Highlighter, Hinter)]
struct InputValidator;

impl Validator for InputValidator {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();

        // Empty lines and ! commands are fine.
        if input.is_empty() || input.starts_with('!') || input == ";" {
            return Ok(ValidationResult::Valid(None));
        }

        // For SQL statements, just look for any semicolon or lexer error and if found accept the
        // input and rely on the server to do further validation and error handling. Otherwise,
        // wait for more input.
        for result in Lexer::new(ctx.input()) {
            match result {
                Ok(Token::Symbol(Symbol::Semicolon)) => return Ok(ValidationResult::Valid(None)),
                Err(_) => return Ok(ValidationResult::Valid(None)),
                _ => {}
            }
        }
        Ok(ValidationResult::Incomplete)
    }

    fn validate_while_typing(&self) -> bool {
        false
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    db_addr: String,
}

impl Config {
    fn new(file: &str) -> Result<Self> {
        let c = config::Config::builder()
            .set_default("db_addr", String::new())?

            .add_source(config::File::with_name(file))
            .add_source(config::Environment::with_prefix("FEATHERDB"));

        Ok(c.build()?.try_deserialize()?)
    }
}