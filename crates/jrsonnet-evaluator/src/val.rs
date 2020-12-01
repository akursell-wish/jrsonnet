use crate::{
	builtin::{
		call_builtin,
		manifest::{manifest_json_ex, ManifestJsonOptions, ManifestType},
	},
	error::Error::*,
	evaluate,
	function::{parse_function_call, parse_function_call_map, place_args},
	native::NativeCallback,
	throw, with_state, Context, ObjValue, Result,
};
use jrsonnet_parser::{el, Arg, ArgsDesc, Expr, ExprLocation, LiteralType, LocExpr, ParamsDesc};
use jrsonnet_types::ValType;
use std::{cell::RefCell, collections::HashMap, fmt::Debug, rc::Rc};

enum LazyValInternals {
	Computed(Val),
	Waiting(Box<dyn Fn() -> Result<Val>>),
}
#[derive(Clone)]
pub struct LazyVal(Rc<RefCell<LazyValInternals>>);
impl LazyVal {
	pub fn new(f: Box<dyn Fn() -> Result<Val>>) -> Self {
		Self(Rc::new(RefCell::new(LazyValInternals::Waiting(f))))
	}
	pub fn new_resolved(val: Val) -> Self {
		Self(Rc::new(RefCell::new(LazyValInternals::Computed(val))))
	}
	pub fn evaluate(&self) -> Result<Val> {
		let new_value = match &*self.0.borrow() {
			LazyValInternals::Computed(v) => return Ok(v.clone()),
			LazyValInternals::Waiting(f) => f()?,
		};
		*self.0.borrow_mut() = LazyValInternals::Computed(new_value.clone());
		Ok(new_value)
	}
}

#[macro_export]
macro_rules! lazy_val {
	($f: expr) => {
		$crate::LazyVal::new(Box::new($f))
	};
}
#[macro_export]
macro_rules! resolved_lazy_val {
	($f: expr) => {
		$crate::LazyVal::new_resolved($f)
	};
}
impl Debug for LazyVal {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Lazy")
	}
}
impl PartialEq for LazyVal {
	fn eq(&self, other: &Self) -> bool {
		Rc::ptr_eq(&self.0, &other.0)
	}
}

#[derive(Debug, PartialEq)]
pub struct FuncDesc {
	pub name: Rc<str>,
	pub ctx: Context,
	pub params: ParamsDesc,
	pub body: LocExpr,
}

#[derive(Debug)]
pub enum FuncVal {
	/// Plain function implemented in jsonnet
	Normal(FuncDesc),
	/// Standard library function
	Intrinsic(Rc<str>),
	/// Library functions implemented in native
	NativeExt(Rc<str>, Rc<NativeCallback>),
}

impl PartialEq for FuncVal {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::Normal(a), Self::Normal(b)) => a == b,
			(Self::Intrinsic(an), Self::Intrinsic(bn)) => an == bn,
			(Self::NativeExt(an, _), Self::NativeExt(bn, _)) => an == bn,
			(..) => false,
		}
	}
}
impl FuncVal {
	pub fn is_ident(&self) -> bool {
		matches!(&self, Self::Intrinsic(n) if n as &str == "id")
	}
	pub fn name(&self) -> Rc<str> {
		match self {
			Self::Normal(normal) => normal.name.clone(),
			Self::Intrinsic(name) => format!("std.{}", name).into(),
			Self::NativeExt(n, _) => format!("native.{}", n).into(),
		}
	}
	pub fn evaluate(
		&self,
		call_ctx: Context,
		loc: &Option<ExprLocation>,
		args: &ArgsDesc,
		tailstrict: bool,
	) -> Result<Val> {
		match self {
			Self::Normal(func) => {
				let ctx = parse_function_call(
					call_ctx,
					Some(func.ctx.clone()),
					&func.params,
					args,
					tailstrict,
				)?;
				evaluate(ctx, &func.body)
			}
			Self::Intrinsic(name) => call_builtin(call_ctx, loc, name, args),
			Self::NativeExt(_name, handler) => {
				let args = parse_function_call(call_ctx, None, &handler.params, args, true)?;
				let mut out_args = Vec::with_capacity(handler.params.len());
				for p in handler.params.0.iter() {
					out_args.push(args.binding(p.0.clone())?.evaluate()?);
				}
				Ok(handler.call(&out_args)?)
			}
		}
	}

	pub fn evaluate_map(
		&self,
		call_ctx: Context,
		args: &HashMap<Rc<str>, Val>,
		tailstrict: bool,
	) -> Result<Val> {
		match self {
			Self::Normal(func) => {
				let ctx = parse_function_call_map(
					call_ctx,
					Some(func.ctx.clone()),
					&func.params,
					args,
					tailstrict,
				)?;
				evaluate(ctx, &func.body)
			}
			Self::Intrinsic(_) => todo!(),
			Self::NativeExt(_, _) => todo!(),
		}
	}

	pub fn evaluate_values(&self, call_ctx: Context, args: &[Val]) -> Result<Val> {
		match self {
			Self::Normal(func) => {
				let ctx = place_args(call_ctx, Some(func.ctx.clone()), &func.params, args)?;
				evaluate(ctx, &func.body)
			}
			Self::Intrinsic(_) => todo!(),
			Self::NativeExt(_, _) => todo!(),
		}
	}
}

#[derive(Clone)]
pub enum ManifestFormat {
	YamlStream(Box<ManifestFormat>),
	Yaml(usize),
	Json(usize),
	ToString,
	String,
}

#[derive(Debug, Clone)]
pub enum ArrValue {
	Lazy(Rc<Vec<LazyVal>>),
	Eager(Rc<Vec<Val>>),
}
impl ArrValue {
	pub fn len(&self) -> usize {
		match self {
			ArrValue::Lazy(l) => l.len(),
			ArrValue::Eager(e) => e.len(),
		}
	}

	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	pub fn get(&self, index: usize) -> Result<Option<Val>> {
		match self {
			ArrValue::Lazy(vec) => {
				if let Some(v) = vec.get(index) {
					Ok(Some(v.evaluate()?))
				} else {
					Ok(None)
				}
			}
			ArrValue::Eager(vec) => Ok(vec.get(index).cloned()),
		}
	}

	pub fn get_lazy(&self, index: usize) -> Option<LazyVal> {
		match self {
			ArrValue::Lazy(vec) => vec.get(index).cloned(),
			ArrValue::Eager(vec) => vec
				.get(index)
				.cloned()
				.map(|val| LazyVal::new_resolved(val)),
		}
	}

	pub fn evaluated(&self) -> Result<Rc<Vec<Val>>> {
		Ok(match self {
			ArrValue::Lazy(vec) => {
				let mut out = Vec::with_capacity(vec.len());
				for item in vec.iter() {
					out.push(item.evaluate()?);
				}
				Rc::new(out)
			}
			ArrValue::Eager(vec) => vec.clone(),
		})
	}

	pub fn iter(&self) -> impl DoubleEndedIterator<Item = Result<Val>> + '_ {
		(0..self.len()).map(move |idx| match self {
			ArrValue::Lazy(l) => l[idx].evaluate(),
			ArrValue::Eager(e) => Ok(e[idx].clone()),
		})
	}

	pub fn iter_lazy(&self) -> impl DoubleEndedIterator<Item = LazyVal> + '_ {
		(0..self.len()).map(move |idx| match self {
			ArrValue::Lazy(l) => l[idx].clone(),
			ArrValue::Eager(e) => LazyVal::new_resolved(e[idx].clone()),
		})
	}

	pub fn reversed(self) -> Self {
		match self {
			ArrValue::Lazy(vec) => {
				let mut out = (&vec as &Vec<_>).clone();
				out.reverse();
				Self::Lazy(Rc::new(out))
			}
			ArrValue::Eager(vec) => {
				let mut out = (&vec as &Vec<_>).clone();
				out.reverse();
				Self::Eager(Rc::new(out))
			}
		}
	}
}

impl From<Vec<LazyVal>> for ArrValue {
	fn from(v: Vec<LazyVal>) -> Self {
		Self::Lazy(Rc::new(v))
	}
}

impl From<Vec<Val>> for ArrValue {
	fn from(v: Vec<Val>) -> Self {
		Self::Eager(Rc::new(v))
	}
}

#[derive(Debug, Clone)]
pub enum Val {
	Bool(bool),
	Null,
	Str(Rc<str>),
	Num(f64),
	Arr(ArrValue),
	Obj(ObjValue),
	Func(Rc<FuncVal>),
}

macro_rules! matches_unwrap {
	($e: expr, $p: pat, $r: expr) => {
		match $e {
			$p => $r,
			_ => panic!("no match"),
			}
	};
}
impl Val {
	/// Creates `Val::Num` after checking for numeric overflow.
	/// As numbers are `f64`, we can just check for their finity.
	pub fn new_checked_num(num: f64) -> Result<Self> {
		if num.is_finite() {
			Ok(Self::Num(num))
		} else {
			throw!(RuntimeError("overflow".into()))
		}
	}

	pub fn assert_type(&self, context: &'static str, val_type: ValType) -> Result<()> {
		let this_type = self.value_type();
		if this_type != val_type {
			throw!(TypeMismatch(context, vec![val_type], this_type))
		} else {
			Ok(())
		}
	}
	pub fn unwrap_num(self) -> Result<f64> {
		Ok(matches_unwrap!(self, Self::Num(v), v))
	}
	pub fn unwrap_func(self) -> Result<Rc<FuncVal>> {
		Ok(matches_unwrap!(self, Self::Func(v), v))
	}
	pub fn try_cast_bool(self, context: &'static str) -> Result<bool> {
		self.assert_type(context, ValType::Bool)?;
		Ok(matches_unwrap!(self, Self::Bool(v), v))
	}
	pub fn try_cast_str(self, context: &'static str) -> Result<Rc<str>> {
		self.assert_type(context, ValType::Str)?;
		Ok(matches_unwrap!(self, Self::Str(v), v))
	}
	pub fn try_cast_num(self, context: &'static str) -> Result<f64> {
		self.assert_type(context, ValType::Num)?;
		self.unwrap_num()
	}
	pub fn value_type(&self) -> ValType {
		match self {
			Self::Str(..) => ValType::Str,
			Self::Num(..) => ValType::Num,
			Self::Arr(..) => ValType::Arr,
			Self::Obj(..) => ValType::Obj,
			Self::Bool(_) => ValType::Bool,
			Self::Null => ValType::Null,
			Self::Func(..) => ValType::Func,
		}
	}

	pub fn to_string(&self) -> Result<Rc<str>> {
		Ok(match self {
			Self::Bool(true) => "true".into(),
			Self::Bool(false) => "false".into(),
			Self::Null => "null".into(),
			Self::Str(s) => s.clone(),
			v => manifest_json_ex(
				&v,
				&ManifestJsonOptions {
					padding: "",
					mtype: ManifestType::ToString,
				},
			)?
			.into(),
		})
	}

	/// Expects value to be object, outputs (key, manifested value) pairs
	pub fn manifest_multi(&self, ty: &ManifestFormat) -> Result<Vec<(Rc<str>, Rc<str>)>> {
		let obj = match self {
			Self::Obj(obj) => obj,
			_ => throw!(MultiManifestOutputIsNotAObject),
		};
		let keys = obj.visible_fields();
		let mut out = Vec::with_capacity(keys.len());
		for key in keys {
			let value = obj
				.get(key.clone())?
				.expect("item in object")
				.manifest(ty)?;
			out.push((key, value));
		}
		Ok(out)
	}

	/// Expects value to be array, outputs manifested values
	pub fn manifest_stream(&self, ty: &ManifestFormat) -> Result<Vec<Rc<str>>> {
		let arr = match self {
			Self::Arr(a) => a,
			_ => throw!(StreamManifestOutputIsNotAArray),
		};
		let mut out = Vec::with_capacity(arr.len());
		for i in arr.iter() {
			out.push(i?.manifest(ty)?);
		}
		Ok(out)
	}

	pub fn manifest(&self, ty: &ManifestFormat) -> Result<Rc<str>> {
		Ok(match ty {
			ManifestFormat::YamlStream(format) => {
				let arr = match self {
					Self::Arr(a) => a,
					_ => throw!(StreamManifestOutputIsNotAArray),
				};
				let mut out = String::new();

				match format as &ManifestFormat {
					ManifestFormat::YamlStream(_) => throw!(StreamManifestOutputCannotBeRecursed),
					ManifestFormat::String => throw!(StreamManifestCannotNestString),
					_ => {}
				};

				if !arr.is_empty() {
					for v in arr.iter() {
						out.push_str("---\n");
						out.push_str(&v?.manifest(format)?);
						out.push('\n');
					}
					out.push_str("...");
				}

				out.into()
			}
			ManifestFormat::Yaml(padding) => self.to_yaml(*padding)?,
			ManifestFormat::Json(padding) => self.to_json(*padding)?,
			ManifestFormat::ToString => self.to_string()?,
			ManifestFormat::String => match self {
				Self::Str(s) => s.clone(),
				_ => throw!(StringManifestOutputIsNotAString),
			},
		})
	}

	/// For manifestification
	pub fn to_json(&self, padding: usize) -> Result<Rc<str>> {
		manifest_json_ex(
			self,
			&ManifestJsonOptions {
				padding: &" ".repeat(padding),
				mtype: if padding == 0 {
					ManifestType::Minify
				} else {
					ManifestType::Manifest
				},
			},
		)
		.map(|s| s.into())
	}

	/// Calls `std.manifestJson`
	#[cfg(feature = "faster")]
	pub fn to_std_json(&self, padding: usize) -> Result<Rc<str>> {
		manifest_json_ex(
			self,
			&ManifestJsonOptions {
				padding: &" ".repeat(padding),
				mtype: ManifestType::Std,
			},
		)
		.map(|s| s.into())
	}

	/// Calls `std.manifestJson`
	#[cfg(not(feature = "faster"))]
	pub fn to_std_json(&self, padding: usize) -> Result<Rc<str>> {
		with_state(|s| {
			let ctx = s
				.create_default_context()?
				.with_var("__tmp__to_json__".into(), self.clone())?;
			Ok(evaluate(
				ctx,
				&el!(Expr::Apply(
					el!(Expr::Index(
						el!(Expr::Var("std".into())),
						el!(Expr::Str("manifestJsonEx".into()))
					)),
					ArgsDesc(vec![
						Arg(None, el!(Expr::Var("__tmp__to_json__".into()))),
						Arg(None, el!(Expr::Str(" ".repeat(padding).into())))
					]),
					false
				)),
			)?
			.try_cast_str("to json")?)
		})
	}
	pub fn to_yaml(&self, padding: usize) -> Result<Rc<str>> {
		with_state(|s| {
			let ctx = s
				.create_default_context()?
				.with_var("__tmp__to_json__".into(), self.clone());
			Ok(evaluate(
				ctx,
				&el!(Expr::Apply(
					el!(Expr::Index(
						el!(Expr::Var("std".into())),
						el!(Expr::Str("manifestYamlDoc".into()))
					)),
					ArgsDesc(vec![
						Arg(None, el!(Expr::Var("__tmp__to_json__".into()))),
						Arg(
							None,
							el!(Expr::Literal(if padding != 0 {
								LiteralType::True
							} else {
								LiteralType::False
							}))
						)
					]),
					false
				)),
			)?
			.try_cast_str("to json")?)
		})
	}
}

const fn is_function_like(val: &Val) -> bool {
	matches!(val, Val::Func(_))
}

/// Native implementation of `std.primitiveEquals`
pub fn primitive_equals(val_a: &Val, val_b: &Val) -> Result<bool> {
	Ok(match (val_a, val_b) {
		(Val::Bool(a), Val::Bool(b)) => a == b,
		(Val::Null, Val::Null) => true,
		(Val::Str(a), Val::Str(b)) => a == b,
		(Val::Num(a), Val::Num(b)) => (a - b).abs() <= f64::EPSILON,
		(Val::Arr(_), Val::Arr(_)) => throw!(RuntimeError(
			"primitiveEquals operates on primitive types, got array".into(),
		)),
		(Val::Obj(_), Val::Obj(_)) => throw!(RuntimeError(
			"primitiveEquals operates on primitive types, got object".into(),
		)),
		(a, b) if is_function_like(&a) && is_function_like(&b) => {
			throw!(RuntimeError("cannot test equality of functions".into()))
		}
		(_, _) => false,
	})
}

/// Native implementation of `std.equals`
pub fn equals(val_a: &Val, val_b: &Val) -> Result<bool> {
	if val_a.value_type() != val_b.value_type() {
		return Ok(false);
	}
	match (val_a, val_b) {
		// Cant test for ptr equality, because all fields needs to be evaluated
		(Val::Arr(a), Val::Arr(b)) => {
			if a.len() != b.len() {
				return Ok(false);
			}
			for (a, b) in a.iter().zip(b.iter()) {
				if !equals(&a?, &b?)? {
					return Ok(false);
				}
			}
			Ok(true)
		}
		(Val::Obj(a), Val::Obj(b)) => {
			let fields = a.visible_fields();
			if fields != b.visible_fields() {
				return Ok(false);
			}
			for field in fields {
				if !equals(&a.get(field.clone())?.unwrap(), &b.get(field)?.unwrap())? {
					return Ok(false);
				}
			}
			Ok(true)
		}
		(a, b) => Ok(primitive_equals(&a, &b)?),
	}
}
