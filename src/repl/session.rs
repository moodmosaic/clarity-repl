use std::collections::{VecDeque, HashMap};
use crate::clarity::types::QualifiedContractIdentifier;
use super::ClarityInterpreter;
use crate::clarity::diagnostic::Diagnostic;
use crate::clarity::functions::NativeFunctions;
use crate::clarity::functions::define::DefineFunctions;
use crate::clarity::variables::NativeVariables;
use crate::clarity::docs::{
    make_api_reference, 
    make_define_reference, 
    make_keyword_reference
};
use ansi_term::{Style, Colour};

enum Command {
    LoadLocalContract(String),
    LoadDeployContract(String),
    UnloadContract(String),
    ExecuteSnippet(String),
    OpenSession,
    CloseSession,
}

pub struct Session {
    session_id: u32,
    started_at: u32,
    commands: VecDeque<Command>,
    defined_functions: VecDeque<Command>,
    defined_contracts: VecDeque<Command>,
    interpreter: ClarityInterpreter,
    api_reference: HashMap<String, String>,
}

impl Session {

    pub fn new() -> Session {
        Session {
            session_id: 0,
            started_at: 0,
            commands: VecDeque::new(),
            defined_functions: VecDeque::new(),
            defined_contracts: VecDeque::new(),
            interpreter: ClarityInterpreter::new(),
            api_reference: build_api_reference(),
        }
    }

    pub fn handle_command(&mut self, command: &str) -> Vec<String> {
        let light_green = Colour::Green.bold();
        let light_red = Colour::Red.bold();
        let light_black = Colour::Black.bold();
        let mut output = Vec::<String>::new();

        match command {
            ".help" => self.display_help(&mut output),
            cmd if cmd.starts_with(".functions") => self.display_functions(&mut output),
            cmd if cmd.starts_with(".doc") => self.display_doc(&mut output, cmd),
            snippet => {
                let result = self.interpret(snippet.to_string());
                match result {
                    Ok(result) => { 
                        output.push(format!("{}", light_green.paint(result)));
                    },
                    Err((message, diagnostic)) => {
                        output.push(format!("{}", light_red.paint(message)));
                        if let Some(diagnostic) = diagnostic {
                            if diagnostic.spans.len() > 0 {
                                let lines = snippet.lines();
                                let mut formatted_lines: Vec<String> = lines.map(|l| l.to_string()).collect();
                                for span in diagnostic.spans {
                                    let first_line = span.start_line as usize - 1;
                                    let last_line = span.end_line as usize - 1;
                                    let mut pass = vec![];

                                    for (line_index, line) in formatted_lines.iter().enumerate() {
                                        if line_index >= first_line && line_index <= last_line {
                                            let (begin, end) = match (line_index == first_line, line_index == last_line) {
                                                (true, true) => (span.start_column as usize - 1, span.end_column as usize - 1), // One line
                                                (true, false) => (span.start_column as usize - 1, line.len() - 1),              // Multiline, first line
                                                (false, false) => (0, line.len() - 1),                                          // Multiline, in between
                                                (false, true) => (0, span.end_column as usize - 1),                             // Multiline, last line 
                                            };
                                            
                                            let error_style = light_red.underline();
                                            let formatted_line = format!("{}{}{}", &line[..begin], error_style.paint(&line[begin..=end]), &line[(end + 1)..]);
                                            pass.push(formatted_line);
                                        } else {
                                            pass.push(line.clone());
                                        }
                                    }
                                    formatted_lines = pass;
                                }
                                output.append(&mut formatted_lines);
                            }
                        }
                    }
                }
            }
        }
        output
    }

    pub fn interpret(&mut self, snippet: String) -> Result<String, (String, Option<Diagnostic>)> {
    
        let contract_identifier = QualifiedContractIdentifier::transient();

        self.interpreter.run(snippet, contract_identifier)
    }

    pub fn lookup_api_reference(&self, keyword: &str) -> Option<&String> {
        self.api_reference.get(keyword)
    }

    pub fn get_api_reference_index(&self) -> Vec<String> {
        let mut keys = self.api_reference.iter()
            .map(|(k, _)| k.to_string())
            .collect::<Vec<String>>();
        keys.sort();
        keys
    }

    fn display_help(&self, output: &mut Vec<String>) {
        let help_colour = Colour::Yellow;
        let coming_soon_colour = Colour::Black.bold();
        output.push(format!("{}", help_colour.paint(".help\t\t\t\tDisplay help")));
        output.push(format!("{}", help_colour.paint(".functions\t\t\tDisplay all the native functions available in clarity")));
        output.push(format!("{}", help_colour.paint(".doc <function> \t\tDisplay documentation for a given native function fn-name")));
        output.push(format!("{}", coming_soon_colour.paint(".mint-stx <principal>\t\tMint STX balance for a given principal [coming soon]")));
        output.push(format!("{}", coming_soon_colour.paint(".get-block-height\t\tGet current block height [coming soon]")));
        output.push(format!("{}", coming_soon_colour.paint(".set-block-height <number>\tSet current block height [coming soon]")));
    }

    fn display_functions(&self, output: &mut Vec<String>) {
        let help_colour = Colour::Yellow;
        let api_reference_index = self.get_api_reference_index();
        output.push(format!("{}", help_colour.paint(api_reference_index.join("\n"))));
    }

    fn display_doc(&self, output: &mut Vec<String>, command: &str) {
        let help_colour = Colour::Yellow;
        let help_accent_colour = Colour::Yellow.bold();
        let keyword = {
            let mut s = command.to_string();
            s = s.replace(".doc", "");
            s = s.replace(" ", "");
            s
        };
        let result = match self.lookup_api_reference(&keyword) {
            Some(doc) => format!("{}", help_colour.paint(doc)),
            None => format!("{}", help_colour.paint("Function unknown")),
        };
        output.push(result);
    }
}

fn build_api_reference() -> HashMap<String, String> {
    let mut api_reference = HashMap::new();
    for func in NativeFunctions::ALL.iter() {
        let api = make_api_reference(&func);
        let description = {
            let mut s = api.description.to_string();
            s = s.replace("\n", " ");
            s
        };
        let doc = format!("Usage\n{}\n\nDescription\n{}\n\nExamples\n{}",
            api.signature, description, api.example);
        api_reference.insert(api.name, doc);
    }

    for func in DefineFunctions::ALL.iter() {
        let api = make_define_reference(&func);
        let description = {
            let mut s = api.description.to_string();
            s = s.replace("\n", " ");
            s
        };
        let doc = format!("Usage\n{}\n\nDescription\n{}\n\nExamples\n{}",
            api.signature, description, api.example);
        api_reference.insert(api.name, doc);
    }

    for func in NativeVariables::ALL.iter() {
        let api = make_keyword_reference(&func);
        let description = {
            let mut s = api.description.to_string();
            s = s.replace("\n", " ");
            s
        };
        let doc = format!("Description\n{}\n\nExamples\n{}",
            description, api.example);
        api_reference.insert(api.name.to_string(), doc);
    }
    api_reference
}
