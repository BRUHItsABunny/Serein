use crate::imports::{self, HostImports, Kind, NoHostImports, status};
use crate::{
	Capability, Error, ExtensionKind, Invocation, MAX_IO_BYTES, MAX_MODULE_BYTES, Manifest, Output,
	Package,
};
use std::collections::BTreeSet;
use wasmi::{
	Caller, Config, EnforcedLimits, Engine, Extern, ExternType, FuncType, Instance, Linker, Module,
	Store, StoreLimits, StoreLimitsBuilder, TrapCode, ValType,
};

const MEMORY_BYTES: usize = 16 * 1024 * 1024;
const FUEL: u64 = 10_000_000;

fn engine() -> Engine {
	let mut config = Config::default();
	config
		.consume_fuel(true)
		.allow_start_fn(false)
		.ignore_custom_sections(true)
		.set_max_recursion_depth(128)
		.set_min_stack_height(4096)
		.set_max_stack_height(256 * 1024)
		.set_max_cached_stacks(0)
		.enforced_limits(EnforcedLimits::strict());
	Engine::new(&config)
}

fn module(engine: &Engine, bytes: &[u8]) -> Result<Module, Error> {
	if bytes.len() > MAX_MODULE_BYTES {
		return Err(Error::Limit);
	}
	let module = Module::new(engine, bytes).map_err(|_| Error::Module)?;
	if !matches!(module.get_export("memory"), Some(ExternType::Memory(_)))
		|| !matches!(module.get_export("serein_alloc"), Some(ExternType::Func(ty)) if ty == FuncType::new([ValType::I32], [ValType::I32]))
		|| !matches!(module.get_export("serein_invoke"), Some(ExternType::Func(ty)) if ty == FuncType::new([ValType::I32, ValType::I32], [ValType::I64]))
	{
		return Err(Error::Module);
	}
	Ok(module)
}

/// Capabilities the module's imports require. Every import must be a known `serein` host
/// function with its exact signature; nothing else may be imported.
fn required_capabilities(module: &Module) -> Result<BTreeSet<Capability>, Error> {
	let mut required = BTreeSet::new();
	for import in module.imports() {
		let host = imports::find(import.name())
			.filter(|_| import.module() == imports::IMPORT_MODULE)
			.ok_or(Error::Module)?;
		let expected = match host.kind {
			Kind::ClockNowMs => FuncType::new([], [ValType::I64]),
			Kind::RandomFill => FuncType::new([ValType::I32, ValType::I32], [ValType::I32]),
		};
		if !matches!(import.ty(), ExternType::Func(ty) if *ty == expected) {
			return Err(Error::Module);
		}
		required.insert(host.capability);
	}
	Ok(required)
}

pub(crate) fn validate_module(bytes: &[u8], manifest: &Manifest) -> Result<(), Error> {
	let engine = engine();
	let module = module(&engine, bytes)?;
	if !required_capabilities(&module)?
		.iter()
		.all(|capability| manifest.capabilities.contains(capability))
	{
		return Err(Error::Capability);
	}
	instantiate(&engine, &module, Box::new(NoHostImports)).map(|_| ())
}

// Classify only engine-owned codes: never surface Wasm-provided text or input/output bytes.
fn execution_error(error: wasmi::Error) -> Error {
	match error.as_trap_code() {
		Some(TrapCode::OutOfFuel) => Error::Fuel,
		Some(TrapCode::GrowthOperationLimited | TrapCode::OutOfSystemMemory) => Error::Memory,
		Some(TrapCode::StackOverflow) => Error::Stack,
		Some(_) => Error::Trap,
		None => match error.kind() {
			wasmi::errors::ErrorKind::Memory(_) | wasmi::errors::ErrorKind::Table(_) => {
				Error::Memory
			}
			_ => Error::Execution,
		},
	}
}

struct HostState {
	limits: StoreLimits,
	services: Box<dyn HostImports>,
	calls: u32,
}

/// Charges one host call plus `bytes` copied, trapping when the call budget or fuel runs out.
fn charge(caller: &mut Caller<'_, HostState>, bytes: u64) -> Result<(), wasmi::Error> {
	let state = caller.data_mut();
	if state.calls >= imports::MAX_HOST_CALLS {
		return Err(TrapCode::OutOfFuel.into());
	}
	state.calls += 1;
	let cost = imports::CALL_FUEL.saturating_add(bytes.saturating_mul(imports::BYTE_FUEL));
	let fuel = caller.get_fuel()?;
	if fuel < cost {
		caller.set_fuel(0)?;
		return Err(TrapCode::OutOfFuel.into());
	}
	caller.set_fuel(fuel - cost)
}

fn linker(engine: &Engine) -> Result<Linker<HostState>, Error> {
	let mut linker = Linker::new(engine);
	let module = imports::IMPORT_MODULE;
	linker
		.func_wrap(
			module,
			"clock_now_ms",
			|mut caller: Caller<'_, HostState>| {
				charge(&mut caller, 0)?;
				Ok(caller
					.data_mut()
					.services
					.now_ms()
					.filter(|ms| *ms >= 0)
					.unwrap_or(-1))
			},
		)
		.map_err(|_| Error::Execution)?;
	linker
		.func_wrap(
			module,
			"random_fill",
			|mut caller: Caller<'_, HostState>, pointer: i32, length: i32| {
				let length = match u32::try_from(length) {
					Ok(length) if length <= imports::MAX_RANDOM_BYTES => length,
					_ => {
						charge(&mut caller, 0)?;
						return Ok(status::INVALID);
					}
				};
				charge(&mut caller, u64::from(length))?;
				let mut bytes = vec![0; length as usize];
				if !caller.data_mut().services.fill_random(&mut bytes) {
					return Ok(status::UNAVAILABLE);
				}
				let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
					return Err(TrapCode::MemoryOutOfBounds.into());
				};
				memory
					.write(&mut caller, pointer as u32 as usize, &bytes)
					.map_err(|_| wasmi::Error::from(TrapCode::MemoryOutOfBounds))?;
				Ok(status::OK)
			},
		)
		.map_err(|_| Error::Execution)?;
	Ok(linker)
}

fn instantiate(
	engine: &Engine,
	module: &Module,
	services: Box<dyn HostImports>,
) -> Result<(Store<HostState>, Instance), Error> {
	let limits = StoreLimitsBuilder::new()
		.memory_size(MEMORY_BYTES)
		.memories(1)
		.tables(1)
		.table_elements(4096)
		.instances(1)
		.trap_on_grow_failure(true)
		.build();
	let mut store = Store::new(
		engine,
		HostState {
			limits,
			services,
			calls: 0,
		},
	);
	store.limiter(|state| &mut state.limits);
	store.set_fuel(FUEL).map_err(|_| Error::Execution)?;
	let instance = linker(engine)?
		.instantiate_and_start(&mut store, module)
		.map_err(execution_error)?;
	Ok((store, instance))
}

/// Execute once without host services; a module importing any host function fails with
/// [`Error::Capability`]. See [`invoke_with`].
pub fn invoke(package: &Package, input: &Invocation) -> Result<Output, Error> {
	invoke_with(package, input, &[], Box::new(NoHostImports))
}

/// Execute once, on the host worker. All Wasm state is dropped before returning.
/// ABI: `serein_alloc(i32) -> i32`, `serein_invoke(i32, i32) -> i64`.
/// The result packs the output pointer in its high 32 bits and byte length in its low 32 bits.
/// Each host function the module imports needs its capability in `grants`; `services`
/// answers those calls for this invocation only.
pub fn invoke_with(
	package: &Package,
	input: &Invocation,
	grants: &[Capability],
	services: Box<dyn HostImports>,
) -> Result<Output, Error> {
	if package.manifest.kind != ExtensionKind::Plugin || package.theme.is_some() {
		return Err(Error::Invalid);
	}
	package.manifest.validate()?;
	input
		.validate(&package.manifest)
		.map_err(|error| match error {
			Error::Invalid => Error::Input,
			Error::Limit => Error::InputLimit,
			other => other,
		})?;
	#[derive(serde::Serialize)]
	struct HostInvocation<'a> {
		#[serde(flatten)]
		input: &'a Invocation,
		host: crate::HostInfo,
	}
	let bytes = serde_json::to_vec(&HostInvocation {
		input,
		host: crate::HostInfo::current(),
	})
	.map_err(|_| Error::Input)?;
	if bytes.len() > MAX_IO_BYTES {
		return Err(Error::InputLimit);
	}
	let engine = engine();
	let module = module(&engine, &package.wasm)?;
	if !required_capabilities(&module)?.iter().all(|capability| {
		grants.contains(capability) && package.manifest.capabilities.contains(capability)
	}) {
		return Err(Error::Capability);
	}
	let (mut store, instance) = instantiate(&engine, &module, services)?;
	let memory = instance.get_memory(&store, "memory").ok_or(Error::Module)?;
	let alloc = instance
		.get_typed_func::<i32, i32>(&store, "serein_alloc")
		.map_err(|_| Error::Module)?;
	let run = instance
		.get_typed_func::<(i32, i32), i64>(&store, "serein_invoke")
		.map_err(|_| Error::Module)?;
	let pointer = alloc
		.call(&mut store, bytes.len() as i32)
		.map_err(execution_error)?;
	memory
		.write(&mut store, pointer as u32 as usize, &bytes)
		.map_err(|_| Error::Trap)?;
	let packed = run
		.call(&mut store, (pointer, bytes.len() as i32))
		.map_err(execution_error)? as u64;
	let length = packed as u32 as usize;
	if length == 0 {
		return Err(Error::Handler);
	}
	if length > MAX_IO_BYTES {
		return Err(Error::OutputLimit);
	}
	let mut bytes = vec![0; length];
	memory
		.read(&store, (packed >> 32) as usize, &mut bytes)
		.map_err(|_| Error::Output)?;
	let output: Output = serde_json::from_slice(&bytes).map_err(|_| Error::Output)?;
	output
		.validate(&package.manifest, input)
		.map_err(|error| match error {
			Error::Limit => Error::OutputLimit,
			Error::Invalid => Error::Output,
			other => other,
		})?;
	Ok(output)
}
