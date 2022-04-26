use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::Hash;
use std::io::Write;
use std::path::PathBuf;

use super::State;
use crate::clarity::callables::FunctionIdentifier;
use crate::clarity::contexts::{ContractContext, GlobalContext};
use crate::clarity::errors::Error;
use crate::clarity::representations::Span;
use crate::clarity::types::{SequenceData, Value};
use crate::clarity::ClarityName;
use crate::clarity::SymbolicExpressionType::List;
use crate::clarity::{
    contexts::{Environment, LocalContext},
    types::QualifiedContractIdentifier,
    EvalHook, SymbolicExpression,
};
use dap_types::events::*;
use dap_types::requests::*;
use dap_types::responses::*;
use dap_types::types::*;
use dap_types::*;
use futures::{SinkExt, StreamExt};
use tokio;
use tokio::io::{Stdin, Stdout};
use tokio::runtime::Runtime;
use tokio_util::codec::{FramedRead, FramedWrite};

use self::codec::{DebugAdapterCodec, ParseError};

use super::DebugState;

mod codec;

/*
 * DAP Session:
 *      VSCode                    DAPDebugger
 *        |                            |
 *        |--- initialize ------------>|
 *        |<-- initialize response ----|
 *        |--- launch ---------------->|
 *        |<-- launch response --------|
 *        |<-- initialized event ------|
 *        |<-- stopped event ----------|
 *        |--- set breakpoints ------->|
 *        |<-- set bps response -------|
 *        |--- threads --------------->|
 *        |<-- threads response -------|
 *        |--- set exception bps ----->|
 *        |<-- set exc bps response ---|
 *        |--- threads --------------->|
 *        |<-- threads response -------|
 */

struct Current {
    source: Source,
    span: Span,
    expr_id: u64,
    stack: Vec<FunctionIdentifier>,
}

pub struct DAPDebugger {
    rt: Runtime,
    pub path_to_contract_id: HashMap<String, QualifiedContractIdentifier>,
    pub contract_id_to_path: HashMap<QualifiedContractIdentifier, String>,
    log_file: File, // DELETE ME: For testing only
    reader: FramedRead<Stdin, DebugAdapterCodec<ProtocolMessage>>,
    writer: FramedWrite<Stdout, DebugAdapterCodec<ProtocolMessage>>,
    state: Option<DebugState>,
    send_seq: i64,
    launched: Option<(String, String)>,
    launch_seq: i64,
    current: Option<Current>,
    init_complete: bool,

    stack_frames: HashMap<FunctionIdentifier, StackFrame>,
    scopes: HashMap<i32, Vec<Scope>>,
    variables: HashMap<i32, Vec<Variable>>,
}

impl DAPDebugger {
    pub fn new() -> Self {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();

        let reader = FramedRead::new(stdin, DebugAdapterCodec::<ProtocolMessage>::default());
        let writer = FramedWrite::new(stdout, DebugAdapterCodec::<ProtocolMessage>::default());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut log_file = match File::create("/Users/brice/work/debugger-demo/dap-log.txt") {
            // DELETE ME
            Ok(file) => file,
            Err(e) => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .append(true)
                    .open("/Users/brice/work/debugger-demo/debugger.txt")
                    .unwrap();
                writeln!(file, "DAP_LOG FAILED: {}", e);
                file
            }
        };
        writeln!(log_file, "LOG FILE CREATED");
        Self {
            rt,
            path_to_contract_id: HashMap::new(),
            contract_id_to_path: HashMap::new(),
            log_file,
            reader,
            writer,
            state: None,
            send_seq: 0,
            launched: None,
            launch_seq: 0,
            current: None,
            init_complete: false,
            stack_frames: HashMap::new(),
            scopes: HashMap::new(),
            variables: HashMap::new(),
        }
    }

    fn get_state(&mut self) -> &mut DebugState {
        self.state.as_mut().unwrap()
    }

    // Process all messages before launching the REPL
    pub fn init(&mut self) -> Result<(String, String), ParseError> {
        writeln!(self.log_file, "STARTING");

        while self.launched.is_none() {
            match self.wait_for_command() {
                Ok(_) => (),
                Err(e) => return Err(e),
            }
        }
        writeln!(
            self.log_file,
            "inited: {}, {}",
            self.launched.as_ref().unwrap().0,
            self.launched.as_ref().unwrap().1
        );
        Ok(self.launched.take().unwrap())
    }

    // Successful result boolean indicates if execution should continue based on the message received
    fn wait_for_command(&mut self) -> Result<bool, ParseError> {
        writeln!(self.log_file, "WAITING FOR MESSAGE...");
        if let Some(msg) = self.rt.block_on(self.reader.next()) {
            match msg {
                Ok(msg) => {
                    writeln!(self.log_file, "message: {:?}", msg);

                    use dap_types::MessageKind::*;
                    Ok(match msg.message {
                        Request(command) => self.handle_request(msg.seq, command),
                        Response(response) => {
                            self.handle_response(msg.seq, response);
                            false
                        }
                        Event(event) => {
                            self.handle_event(msg.seq, event);
                            false
                        }
                    })
                }
                Err(e) => {
                    writeln!(self.log_file, "error: {}", e);
                    Err(e)
                }
            }
        } else {
            writeln!(self.log_file, "NONE");
            Ok(true)
        }
    }

    fn send_response(&mut self, response: Response) {
        let response_json = serde_json::to_string(&response).unwrap();
        writeln!(self.log_file, "::::response: {}", response_json);

        let message = ProtocolMessage {
            seq: self.send_seq,
            message: MessageKind::Response(response),
        };

        match self.rt.block_on(self.writer.send(message)) {
            Ok(_) => (),
            Err(e) => {
                writeln!(self.log_file, "ERROR: sending response: {}", e);
            }
        };

        self.send_seq += 1;
    }

    fn send_event(&mut self, body: EventBody) {
        let event_json = serde_json::to_string(&body).unwrap();
        writeln!(self.log_file, "::::event: {}", event_json);

        let message = ProtocolMessage {
            seq: self.send_seq,
            message: MessageKind::Event(Event { body: Some(body) }),
        };

        match self.rt.block_on(self.writer.send(message)) {
            Ok(_) => (),
            Err(e) => {
                writeln!(self.log_file, "ERROR: sending response: {}", e);
            }
        };

        self.send_seq += 1;
    }

    pub fn handle_request(&mut self, seq: i64, command: RequestCommand) -> bool {
        use dap_types::requests::RequestCommand::*;
        let proceed = match command {
            Initialize(arguments) => self.initialize(seq, arguments),
            Launch(arguments) => self.launch(seq, arguments),
            ConfigurationDone => self.configuration_done(seq),
            SetBreakpoints(arguments) => self.setBreakpoints(seq, arguments),
            SetExceptionBreakpoints(arguments) => self.setExceptionBreakpoints(seq, arguments),
            Disconnect(arguments) => self.quit(seq, arguments),
            Threads => self.threads(seq),
            StackTrace(arguments) => self.stack_trace(seq, arguments),
            Scopes(arguments) => self.scopes(seq, arguments),
            Variables(arguments) => self.variables(seq, arguments),
            StepIn(arguments) => self.step_in(seq, arguments),
            StepOut(arguments) => self.step_out(seq, arguments),
            Next(arguments) => self.next(seq, arguments),
            Continue(arguments) => self.continue_(seq, arguments),
            Pause(arguments) => self.pause(seq, arguments),
            _ => {
                self.send_response(Response {
                    request_seq: seq,
                    success: false,
                    message: Some("unsupported request".to_string()),
                    body: None,
                });
                false
            }
        };

        proceed
    }

    pub fn handle_event(&mut self, seq: i64, event: Event) {
        let response = Response {
            request_seq: seq,
            success: true,
            message: None,
            body: None,
        };
        self.send_response(response);
    }

    pub fn handle_response(&mut self, seq: i64, response: Response) {
        let response = Response {
            request_seq: seq,
            success: true,
            message: None,
            body: None,
        };
        self.send_response(response);
    }

    // Request handlers

    fn initialize(&mut self, seq: i64, arguments: InitializeRequestArguments) -> bool {
        writeln!(self.log_file, "INITIALIZE");
        let capabilities = Capabilities {
            supports_configuration_done_request: Some(true),
            supports_function_breakpoints: Some(true),
            supports_step_in_targets_request: Some(true),
            support_terminate_debuggee: Some(true),
            supports_loaded_sources_request: Some(true),
            supports_data_breakpoints: Some(true),
            supports_breakpoint_locations_request: Some(true),
            supports_conditional_breakpoints: None,
            supports_hit_conditional_breakpoints: None,
            supports_evaluate_for_hovers: None,
            exception_breakpoint_filters: None,
            supports_step_back: None,
            supports_set_variable: None,
            supports_restart_frame: None,
            supports_goto_targets_request: None,
            supports_completions_request: None,
            completion_trigger_characters: None,
            supports_modules_request: None,
            additional_module_columns: None,
            supported_checksum_algorithms: None,
            supports_restart_request: None,
            supports_exception_options: None,
            supports_value_formatting_options: None,
            supports_exception_info_request: None,
            support_suspend_debuggee: None,
            supports_delayed_stack_trace_loading: None,
            supports_log_points: None,
            supports_terminate_threads_request: None,
            supports_set_expression: None,
            supports_terminate_request: None,
            supports_read_memory_request: None,
            supports_write_memory_request: None,
            supports_disassemble_request: None,
            supports_cancel_request: None,
            supports_clipboard_context: None,
            supports_stepping_granularity: None,
            supports_instruction_breakpoints: None,
            supports_exception_filter_options: None,
            supports_single_thread_execution_requests: None,
        };

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Initialize(InitializeResponse {
                capabilities,
            })),
        });

        false
    }

    pub fn log<S: Into<String>>(&mut self, message: S) {
        self.send_event(EventBody::Output(OutputEvent {
            category: Some(Category::Console),
            output: message.into(),
            group: None,
            variables_reference: None,
            source: None,
            line: None,
            column: None,
            data: None,
        }));
    }

    pub fn stdout<S: Into<String>>(&mut self, message: S) {
        self.send_event(EventBody::Output(OutputEvent {
            category: Some(Category::Stdout),
            output: message.into(),
            group: None,
            variables_reference: None,
            source: None,
            line: None,
            column: None,
            data: None,
        }));
    }

    pub fn stderr<S: Into<String>>(&mut self, message: S) {
        self.send_event(EventBody::Output(OutputEvent {
            category: Some(Category::Stderr),
            output: message.into(),
            group: None,
            variables_reference: None,
            source: None,
            line: None,
            column: None,
            data: None,
        }));
    }

    fn launch(&mut self, seq: i64, arguments: LaunchRequestArguments) -> bool {
        writeln!(self.log_file, "LAUNCH");
        // Verify that the manifest and expression were specified
        let manifest = match arguments.manifest {
            Some(manifest) => manifest,
            None => {
                self.send_response(Response {
                    request_seq: seq,
                    success: false,
                    message: Some("manifest must be specified".to_string()),
                    body: None,
                });
                return false;
            }
        };
        let expression = match arguments.expression {
            Some(expression) => expression,
            None => {
                self.send_response(Response {
                    request_seq: seq,
                    success: false,
                    message: Some("expression to debug must be specified".to_string()),
                    body: None,
                });
                return false;
            }
        };

        let contract_id = QualifiedContractIdentifier::transient();
        self.state = Some(DebugState::new(&contract_id, &expression));
        self.launched = Some((manifest, expression));

        self.launch_seq = seq;

        false
    }

    fn configuration_done(&mut self, seq: i64) -> bool {
        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::ConfigurationDone),
        });

        // Now that configuration is done, we can respond to the launch
        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Launch),
        });

        false
    }

    fn setBreakpoints(&mut self, seq: i64, arguments: SetBreakpointsArguments) -> bool {
        let mut results = vec![];
        match arguments.breakpoints {
            Some(breakpoints) => {
                let source = super::Source {
                    name: self
                        .path_to_contract_id
                        .get(&arguments.source.path.clone().unwrap())
                        .unwrap()
                        .clone(),
                };
                for breakpoint in breakpoints {
                    let column = match breakpoint.column {
                        Some(column) => column,
                        None => 0,
                    };
                    let source_breakpoint = super::Breakpoint {
                        id: 0,
                        verified: true,
                        data: super::BreakpointData::Source(super::SourceBreakpoint {
                            line: breakpoint.line,
                            column: breakpoint.column,
                        }),
                        source: source.clone(),
                        span: Some(Span {
                            start_line: breakpoint.line,
                            start_column: column,
                            end_line: breakpoint.line,
                            end_column: column,
                        }),
                    };
                    let id = self.get_state().add_breakpoint(source_breakpoint);
                    results.push(Breakpoint {
                        id: Some(id),
                        verified: true,
                        message: breakpoint.log_message,
                        source: Some(arguments.source.clone()),
                        line: Some(breakpoint.line),
                        column: breakpoint.column,
                        end_line: Some(breakpoint.line),
                        end_column: breakpoint.column,
                        instruction_reference: None,
                        offset: None,
                    });
                }
            }
            None => (),
        };

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::SetBreakpoints(SetBreakpointsResponse {
                breakpoints: results,
            })),
        });

        false
    }

    fn setExceptionBreakpoints(
        &mut self,
        seq: i64,
        arguments: SetExceptionBreakpointsArguments,
    ) -> bool {
        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::SetExceptionBreakpoints(
                SetExceptionBreakpointsResponse { breakpoints: None },
            )),
        });

        false
    }

    fn threads(&mut self, seq: i64) -> bool {
        // There is only ever 1 thread
        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Threads(ThreadsResponse {
                threads: vec![Thread {
                    id: 0,
                    name: "main".to_string(),
                }],
            })),
        });

        // VSCode doesn't seem to want to send us a ConfigurationDone request,
        // so assume that this is the end of configuration instead. This is an
        // ugly hack and should be changed!
        if !self.init_complete {
            self.send_response(Response {
                request_seq: self.launch_seq,
                success: true,
                message: None,
                body: Some(ResponseBody::Launch),
            });

            let mut stopped = StoppedEvent {
                reason: StoppedReason::Entry,
                description: None,
                thread_id: Some(0),
                preserve_focus_hint: None,
                text: Some("Stopped at start!!!".to_string()),
                all_threads_stopped: None,
                hit_breakpoint_ids: None,
            };

            self.send_event(EventBody::Stopped(stopped));
            self.init_complete = true;
        }

        false
    }

    fn stack_trace(&mut self, seq: i64, arguments: StackTraceArguments) -> bool {
        let current = self.current.as_ref().unwrap();
        let frames: Vec<_> = current
            .stack
            .iter()
            .rev()
            .filter(|function| !function.identifier.starts_with("_native_:"))
            .map(|function| self.stack_frames[function].clone())
            .collect();

        let len = current.stack.len() as i32;
        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::StackTrace(StackTraceResponse {
                stack_frames: frames,
                total_frames: Some(len),
            })),
        });
        false
    }

    fn scopes(&mut self, seq: i64, arguments: ScopesArguments) -> bool {
        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Scopes(ScopesResponse {
                scopes: self.scopes[&arguments.frame_id].clone(),
            })),
        });
        false
    }

    fn variables(&mut self, seq: i64, arguments: VariablesArguments) -> bool {
        let variables = match self.variables.get(&arguments.variables_reference) {
            Some(variables) => variables.clone(),
            None => {
                self.log("unknown variable reference");
                Vec::new()
            }
        };

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Variables(VariablesResponse { variables })),
        });
        false
    }

    fn step_in(&mut self, seq: i64, arguments: StepInArguments) -> bool {
        self.get_state().step_in();

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::StepIn),
        });
        true
    }

    fn step_out(&mut self, seq: i64, arguments: StepOutArguments) -> bool {
        self.get_state().finish();

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::StepOut),
        });
        true
    }

    fn next(&mut self, seq: i64, arguments: NextArguments) -> bool {
        let expr_id = self.current.as_ref().unwrap().expr_id;
        self.get_state().step_over(expr_id);

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Next),
        });
        true
    }

    fn continue_(&mut self, seq: i64, arguments: ContinueArguments) -> bool {
        self.get_state().continue_execution();

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Continue(ContinueResponse {
                all_threads_continued: None,
            })),
        });
        true
    }

    fn pause(&mut self, seq: i64, arguments: PauseArguments) -> bool {
        self.get_state().pause();

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Pause),
        });
        true
    }

    fn quit(&mut self, seq: i64, arguments: DisconnectArguments) -> bool {
        // match arguments.restart {
        //     Some(restart) => restart,
        //     None => false,
        // }
        self.get_state().quit();

        self.send_response(Response {
            request_seq: seq,
            success: true,
            message: None,
            body: Some(ResponseBody::Disconnect),
        });
        true
    }

    fn save_scopes_for_frame(
        &mut self,
        stack_frame: &StackFrame,
        local_context: &LocalContext,
        contract_context: &ContractContext,
        global_context: &mut GlobalContext,
    ) {
        let mut scopes = Vec::new();
        let mut current_context = Some(local_context);
        writeln!(
            self.log_file,
            "initial context: variables: {:?}, callable_contracts: {:?}, depth: {}, parent: {}, function vars: {}",
            local_context.variables,
            local_context.callable_contracts,
            local_context.depth(),
            local_context.parent.is_some(),
            local_context.function_context().variables.len(),
        );
        writeln!(
            self.log_file,
            "contract variables: {:?}",
            contract_context.variables
        );

        // Local variable scopes
        while let Some(ctx) = current_context {
            let scope_id = stack_frame.id * 1000 + (scopes.len() as i32);
            scopes.push(Scope {
                name: if ctx.depth() == 0 {
                    "Arguments"
                } else {
                    "Locals"
                }
                .to_string(),
                presentation_hint: if ctx.depth() == 0 {
                    Some(PresentationHint::Arguments)
                } else {
                    Some(PresentationHint::Locals)
                },
                variables_reference: scope_id,
                named_variables: Some(ctx.variables.len() + ctx.callable_contracts.len()),
                indexed_variables: None,
                expensive: false,
                source: stack_frame.source.clone(),
                line: None,
                column: None,
                end_line: None,
                end_column: None,
            });
            writeln!(
                self.log_file,
                "created scope {} with {} variables",
                scope_id,
                ctx.variables.len()
            );

            let mut variables = Vec::new();
            for (name, value) in &ctx.variables {
                variables.push(Variable {
                    name: name.to_string(),
                    value: value.to_string(),
                    var_type: Some(type_for_value(value)),
                    presentation_hint: None,
                    evaluate_name: None,
                    variables_reference: 0,
                    named_variables: None,
                    indexed_variables: None,
                    memory_reference: None,
                });
            }
            for (name, (contract_id, trait_id)) in &ctx.callable_contracts {
                variables.push(Variable {
                    name: name.to_string(),
                    // value: format!("{}.{}", contract_id, trait_id),
                    value: format!("{}", contract_id),
                    var_type: Some(format!("{}", trait_id)),
                    presentation_hint: None,
                    evaluate_name: None,
                    variables_reference: 0,
                    named_variables: None,
                    indexed_variables: None,
                    memory_reference: None,
                });
            }
            self.variables.insert(scope_id, variables);

            current_context = ctx.parent;
        }

        // Contract global scope
        let scope_id = stack_frame.id * 1000 + (scopes.len() as i32);
        scopes.push(Scope {
            name: "Contract Variables".to_string(),
            presentation_hint: None,
            variables_reference: scope_id,
            named_variables: Some(contract_context.variables.len()),
            indexed_variables: None,
            expensive: true,
            source: stack_frame.source.clone(),
            line: None,
            column: None,
            end_line: None,
            end_column: None,
        });
        let mut variables = Vec::new();

        // Constants
        for (name, value) in &contract_context.variables {
            variables.push(Variable {
                name: name.to_string(),
                value: value.to_string(),
                var_type: Some(type_for_value(value)),
                presentation_hint: None,
                evaluate_name: None,
                variables_reference: 0,
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
            });
        }

        // Variables
        for (name, metadata) in &contract_context.meta_data_var {
            let data_types = contract_context.meta_data_var.get(name).unwrap();
            let value = global_context
                .database
                .lookup_variable(
                    &contract_context.contract_identifier,
                    name.as_str(),
                    data_types,
                )
                .unwrap();
            variables.push(Variable {
                name: name.to_string(),
                value: value.to_string(),
                var_type: Some(format!("{}", metadata.value_type)),
                presentation_hint: None,
                evaluate_name: None,
                variables_reference: 0,
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
            });
        }

        // Maps
        for (name, metadata) in &contract_context.meta_data_map {
            // We do not grab any values for maps. Users can query map values.
            variables.push(Variable {
                name: name.to_string(),
                value: "".to_string(),
                var_type: Some(format!(
                    "{{{}: {}}}",
                    metadata.key_type, metadata.value_type
                )),
                presentation_hint: None,
                evaluate_name: None,
                variables_reference: 0,
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
            });
        }
        self.variables.insert(scope_id, variables);

        self.scopes.insert(stack_frame.id, scopes);
    }
}

impl EvalHook for DAPDebugger {
    fn begin_eval(
        &mut self,
        env: &mut Environment,
        context: &LocalContext,
        expr: &SymbolicExpression,
    ) {
        writeln!(self.log_file, "in begin_eval: {:?}", expr);

        let source = Source {
            name: Some(env.contract_context.contract_identifier.to_string()),
            path: Some(
                match self
                    .contract_id_to_path
                    .get(&env.contract_context.contract_identifier)
                {
                    Some(path) => path.clone(),
                    _ => "debugger".to_string(),
                },
            ),
            source_reference: None,
            presentation_hint: None,
            origin: None,
            sources: None,
            adapter_data: None,
            checksums: None,
        };

        // Find the current function frame, ignoring builtin functions.
        let mut current_function = None;
        for function in env.call_stack.stack.iter().rev() {
            if !function.identifier.starts_with("_native_:") {
                current_function = Some(function);
                break;
            }
        }
        if let Some(current_function) = current_function {
            if let Some(mut stack_top) = self.stack_frames.remove(current_function) {
                stack_top.line = expr.span.start_line;
                stack_top.column = expr.span.start_column;
                stack_top.end_line = Some(expr.span.end_line);
                stack_top.end_column = Some(expr.span.end_column);

                writeln!(self.log_file, "updated stack frame {}", stack_top.id);
                self.save_scopes_for_frame(
                    &stack_top,
                    context,
                    &env.contract_context,
                    &mut env.global_context,
                );
                writeln!(self.log_file, "{:?}", env.contract_context.variables);
                self.stack_frames
                    .insert(current_function.clone(), stack_top);
            } else {
                let stack_frame = StackFrame {
                    id: (env.call_stack.stack.len() as i32 + 1),
                    name: current_function.identifier.clone(),
                    source: Some(source.clone()),
                    line: expr.span.start_line,
                    column: expr.span.start_column,
                    end_line: Some(expr.span.end_line),
                    end_column: Some(expr.span.end_column),
                    can_restart: None,
                    instruction_pointer_reference: None,
                    module_id: None,
                    presentation_hint: Some(PresentationHint::Normal),
                };
                writeln!(self.log_file, "created stack frame {}", stack_frame.id);
                self.save_scopes_for_frame(
                    &stack_frame,
                    context,
                    &env.contract_context,
                    &mut env.global_context,
                );

                self.stack_frames
                    .insert(current_function.clone(), stack_frame);
            }
        }

        if !self.get_state().begin_eval(env, context, expr) {
            if self.get_state().state == State::Start {
                // Sending this initialized event triggers the configuration
                // (e.g. setting breakpoints), after which the ConfigurationDone
                // request should be sent, but it's not, so there is an ugly
                // hack in threads to handle that.
                self.send_event(EventBody::Initialized);
            } else {
                let mut stopped = StoppedEvent {
                    reason: StoppedReason::Entry,
                    description: None,
                    thread_id: Some(0),
                    preserve_focus_hint: None,
                    text: None,
                    all_threads_stopped: None,
                    hit_breakpoint_ids: None,
                };

                let state = self.get_state().state.clone();
                writeln!(self.log_file, "STATE: {:?}", state);

                match self.get_state().state {
                    State::Start => {
                        stopped.reason = StoppedReason::Entry;
                    }
                    State::Break(breakpoint) => {
                        stopped.reason = StoppedReason::Breakpoint;
                        stopped.hit_breakpoint_ids = Some(vec![breakpoint]);
                    }
                    State::DataBreak(breakpoint, access_type) => {
                        stopped.reason = StoppedReason::DataBreakpoint;
                        stopped.hit_breakpoint_ids = Some(vec![breakpoint]);
                    }
                    State::Finished | State::StepIn | State::StepOver(_) => {
                        stopped.reason = StoppedReason::Step;
                    }
                    State::Pause => {
                        stopped.reason = StoppedReason::Pause;
                    }
                    _ => unreachable!("Unexpected state"),
                };
                self.send_event(EventBody::Stopped(stopped));
            }

            writeln!(self.log_file, "  wait for command");
            writeln!(self.log_file, "stack: {:?}", env.call_stack.stack);

            // Save the current state, which may be needed to respond to incoming requests
            self.current = Some(Current {
                source,
                span: expr.span.clone(),
                expr_id: expr.id,
                stack: env.call_stack.stack.clone(),
            });

            let mut proceed = false;
            while !proceed {
                proceed = match self.wait_for_command() {
                    Ok(proceed) => proceed,
                    Err(e) => {
                        writeln!(self.log_file, "  ERROR: {}", e);
                        false
                    }
                };
            }
            self.current = None;
        } else {
            // TODO: If there is already a message waiting, process it before continuing.
            // Something with self.reader.poll_read() maybe?

            writeln!(self.log_file, "  continue");
        }
    }

    fn finish_eval(
        &mut self,
        env: &mut Environment,
        context: &LocalContext,
        expr: &SymbolicExpression,
        res: &Result<Value, Error>,
    ) {
        writeln!(self.log_file, "in finish_eval: {}", expr.id);
        if self.get_state().finish_eval(env, context, expr, res) {}
    }

    fn complete(&mut self, result: Result<(Value, Vec<serde_json::Value>), String>) {
        match result {
            Ok((result, events)) => {
                self.log("Execution completed.\n");
                if !events.is_empty() {
                    self.log("\nEvents emitted:\n");
                    for event in events {
                        self.stdout(format!("{}\n", event));
                    }
                }

                self.log("\nReturn value:");
                self.stdout(format!("{}\n", result))
            }
            Err(e) => self.stderr(e),
        }
    }
}

fn type_for_value(value: &Value) -> String {
    match value {
        Value::Int(int) => "int".to_string(),
        Value::UInt(int) => "uint".to_string(),
        Value::Bool(boolean) => "bool".to_string(),
        Value::Tuple(data) => format!("{}", data.type_signature),
        Value::Principal(principal_data) => "principal".to_string(),
        Value::Optional(opt_data) => format!("{}", opt_data.type_signature()),
        Value::Response(res_data) => format!("{}", res_data.type_signature()),
        Value::Sequence(SequenceData::Buffer(vec_bytes)) => "buff".to_string(),
        Value::Sequence(SequenceData::String(string)) => "string".to_string(),
        Value::Sequence(SequenceData::List(list_data)) => "list".to_string(),
    }
}
