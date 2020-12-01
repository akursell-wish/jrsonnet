use crate::{error::Error::*, evaluate, lazy_val, resolved_lazy_val, throw, Context, Result, Val};
use closure::closure;
use jrsonnet_parser::{ArgsDesc, ParamsDesc};
use rustc_hash::FxHashMap;
use std::{collections::HashMap, hash::BuildHasherDefault, rc::Rc};

const NO_DEFAULT_CONTEXT: &str =
	"no default context set for call with defined default parameter value";

/// Creates correct [context](Context) for function body evaluation returning error on invalid call.
///
/// ## Parameters
/// * `ctx`: used for passed argument expressions' execution and for body execution (if `body_ctx` is not set)
/// * `body_ctx`: used for default parameter values' execution and for body execution (if set)
/// * `params`: function parameters' definition
/// * `args`: passed function arguments
/// * `tailstrict`: if set to `true` function arguments are eagerly executed, otherwise - lazily
pub fn parse_function_call(
	ctx: Context,
	body_ctx: Option<Context>,
	params: &ParamsDesc,
	args: &ArgsDesc,
	tailstrict: bool,
) -> Result<Context> {
	let mut out = HashMap::with_capacity_and_hasher(params.len(), BuildHasherDefault::default());
	let mut positioned_args = vec![None; params.0.len()];
	for (id, arg) in args.iter().enumerate() {
		let idx = if let Some(name) = &arg.0 {
			params
				.iter()
				.position(|p| *p.0 == *name)
				.ok_or_else(|| UnknownFunctionParameter(name.clone()))?
		} else {
			id
		};

		if idx >= params.len() {
			throw!(TooManyArgsFunctionHas(params.len()));
		}
		if positioned_args[idx].is_some() {
			throw!(BindingParameterASecondTime(params[idx].0.clone()));
		}
		positioned_args[idx] = Some(arg.1.clone());
	}
	// Fill defaults
	for (id, p) in params.iter().enumerate() {
		let (ctx, expr) = if let Some(arg) = &positioned_args[id] {
			(ctx.clone(), arg)
		} else if let Some(default) = &p.1 {
			(body_ctx.clone().expect(NO_DEFAULT_CONTEXT), default)
		} else {
			throw!(FunctionParameterNotBoundInCall(p.0.clone()));
		};
		let val = if tailstrict {
			resolved_lazy_val!(evaluate(ctx, expr)?)
		} else {
			lazy_val!(closure!(clone ctx, clone expr, ||evaluate(ctx.clone(), &expr)))
		};
		out.insert(p.0.clone(), val);
	}

	Ok(body_ctx.unwrap_or(ctx).extend(out, None, None, None))
}

pub fn parse_function_call_map(
	ctx: Context,
	body_ctx: Option<Context>,
	params: &ParamsDesc,
	args: &HashMap<Rc<str>, Val>,
	tailstrict: bool,
) -> Result<Context> {
	let mut out = FxHashMap::with_capacity_and_hasher(params.len(), BuildHasherDefault::default());
	let mut positioned_args = vec![None; params.0.len()];
	for (name, val) in args.iter() {
		let idx = params
			.iter()
			.position(|p| *p.0 == **name)
			.ok_or_else(|| UnknownFunctionParameter((name as &str).to_owned()))?;

		if idx >= params.len() {
			throw!(TooManyArgsFunctionHas(params.len()));
		}
		if positioned_args[idx].is_some() {
			throw!(BindingParameterASecondTime(params[idx].0.clone()));
		}
		positioned_args[idx] = Some(val.clone());
	}
	// Fill defaults
	for (id, p) in params.iter().enumerate() {
		let val = if let Some(arg) = positioned_args[id].take() {
			resolved_lazy_val!(arg)
		} else if let Some(default) = &p.1 {
			if tailstrict {
				resolved_lazy_val!(evaluate(
					body_ctx.clone().expect(NO_DEFAULT_CONTEXT),
					default
				)?)
			} else {
				let body_ctx = body_ctx.clone();
				let default = default.clone();
				lazy_val!(move || {
					evaluate(body_ctx.clone().expect(NO_DEFAULT_CONTEXT), &default)
				})
			}
		} else {
			throw!(FunctionParameterNotBoundInCall(p.0.clone()));
		};
		out.insert(p.0.clone(), val);
	}

	Ok(body_ctx.unwrap_or(ctx).extend(out, None, None, None))
}

pub fn place_args(
	ctx: Context,
	body_ctx: Option<Context>,
	params: &ParamsDesc,
	args: &[Val],
) -> Result<Context> {
	let mut out = FxHashMap::with_capacity_and_hasher(params.len(), BuildHasherDefault::default());
	let mut positioned_args = vec![None; params.0.len()];
	for (id, arg) in args.iter().enumerate() {
		if id >= params.len() {
			throw!(TooManyArgsFunctionHas(params.len()));
		}
		positioned_args[id] = Some(arg);
	}
	// Fill defaults
	for (id, p) in params.iter().enumerate() {
		let val = if let Some(arg) = &positioned_args[id] {
			(*arg).clone()
		} else if let Some(default) = &p.1 {
			evaluate(ctx.clone(), default)?
		} else {
			throw!(FunctionParameterNotBoundInCall(p.0.clone()));
		};
		out.insert(p.0.clone(), resolved_lazy_val!(val));
	}

	Ok(body_ctx.unwrap_or(ctx).extend(out, None, None, None))
}

#[macro_export]
macro_rules! parse_args {
	($ctx: expr, $fn_name: expr, $args: expr, $total_args: expr, [
		$($id: expr, $name: ident: $ty: expr $(=>$match: path)?);+ $(;)?
	], $handler:block) => {{
		let args = $args;
		if args.len() > $total_args {
			throw!(TooManyArgsFunctionHas($total_args));
		}
		$(
			if args.len() <= $id {
				throw!(FunctionParameterNotBoundInCall(stringify!($name).into()));
			}
			let $name = &args[$id];
			if $name.0.is_some() {
				if $name.0.as_ref().unwrap() != stringify!($name) {
					throw!(IntrinsicArgumentReorderingIsNotSupportedYet);
				}
			}
			let $name = push(&None, || format!("evaluating argument"), || {
				let value = evaluate($ctx.clone(), &$name.1)?;
				$ty.check(&value)?;
				Ok(value)
			})?;
			$(
				let $name = if let $match(v) = $name {
					v
				} else {
					unreachable!();
				};
			)?
		)+
		($handler as crate::Result<_>)
	}};
}
