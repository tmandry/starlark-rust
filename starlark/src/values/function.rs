// Copyright 2018 The Starlark in Rust Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Function as a TypedValue
use super::*;
use crate::stdlib::macros::param::TryParamConvertFromValue;
use crate::values::error::RuntimeError;
use crate::values::none::NoneType;
use std::convert::TryInto;
use std::iter;
use std::mem;

#[derive(Debug, Clone)]
#[doc(hidden)]
pub enum FunctionParameter {
    Normal(String),
    Optional(String),
    WithDefaultValue(String, Value),
    ArgsArray(String),
    KWArgsDict(String),
}

#[derive(Debug, Clone)]
#[doc(hidden)]
pub enum FunctionType {
    Native(String),
    Def(String, String),
}

#[derive(Debug, Clone)]
pub enum FunctionArg {
    Normal(Value),
    Optional(Option<Value>),
    ArgsArray(Vec<Value>),
    KWArgsDict(LinkedHashMap<String, Value>),
}

impl FunctionArg {
    pub fn into_normal<T: TryParamConvertFromValue>(
        self,
        param_name: &'static str,
    ) -> Result<T, ValueError> {
        match self {
            FunctionArg::Normal(v) => {
                T::try_from(v).map_err(|_| ValueError::IncorrectParameterTypeNamed(param_name))
            }
            _ => Err(ValueError::IncorrectParameterType),
        }
    }

    pub fn into_optional<T: TryParamConvertFromValue>(
        self,
        param_name: &'static str,
    ) -> Result<Option<T>, ValueError> {
        match self {
            FunctionArg::Optional(Some(v)) => {
                Ok(Some(T::try_from(v).map_err(|_| {
                    ValueError::IncorrectParameterTypeNamed(param_name)
                })?))
            }
            FunctionArg::Optional(None) => Ok(None),
            _ => Err(ValueError::IncorrectParameterType),
        }
    }

    pub fn into_args_array<T: TryParamConvertFromValue>(
        self,
        param_name: &'static str,
    ) -> Result<Vec<T>, ValueError> {
        match self {
            FunctionArg::ArgsArray(v) => Ok(v
                .into_iter()
                .map(T::try_from)
                .collect::<Result<Vec<T>, _>>()
                .map_err(|_| ValueError::IncorrectParameterTypeNamed(param_name))?),
            _ => Err(ValueError::IncorrectParameterType),
        }
    }

    pub fn into_kw_args_dict<T: TryParamConvertFromValue>(
        self,
        param_name: &'static str,
    ) -> Result<LinkedHashMap<String, T>, ValueError> {
        match self {
            FunctionArg::KWArgsDict(dict) => Ok({
                let mut r = LinkedHashMap::new();
                for (k, v) in dict {
                    r.insert(
                        k,
                        T::try_from(v)
                            .map_err(|_| ValueError::IncorrectParameterTypeNamed(param_name))?,
                    );
                }
                r
            }),
            _ => Err(ValueError::IncorrectParameterType),
        }
    }
}

impl From<FunctionArg> for Value {
    fn from(a: FunctionArg) -> Value {
        match a {
            FunctionArg::Normal(v) => v,
            FunctionArg::ArgsArray(v) => v.into(),
            FunctionArg::Optional(v) => match v {
                Some(v) => v,
                None => Value::new(NoneType::None),
            },
            FunctionArg::KWArgsDict(v) => {
                // `unwrap` does not panic, because key is a string which hashable
                v.try_into().unwrap()
            }
        }
    }
}

pub type StarlarkFunctionPrototype =
    dyn Fn(&CallStack, TypeValues, Vec<FunctionArg>) -> ValueResult;

/// Function implementation for native (written in Rust) functions.
///
/// Public to be referenced in macros.
#[doc(hidden)]
pub struct NativeFunction {
    /// Pointer to a native function.
    /// Note it is a function pointer, not `Box<Fn(...)>`
    /// to avoid generic instantiation and allocation for each native function.
    function: fn(&CallStack, TypeValues, Vec<FunctionArg>) -> ValueResult,
    signature: Vec<FunctionParameter>,
    function_type: FunctionType,
}

// Wrapper for method that have been affected the self object
pub(crate) struct WrappedMethod {
    method: Value,
    self_obj: Value,
}

// TODO: move that code in some common error code list?
// CV prefix = Critical Function call
const NOT_ENOUGH_PARAMS_ERROR_CODE: &str = "CF00";
const WRONG_ARGS_IDENT_ERROR_CODE: &str = "CF01";
const ARGS_NOT_ITERABLE_ERROR_CODE: &str = "CF02";
const KWARGS_NOT_MAPPABLE_ERROR_CODE: &str = "CF03";
// Not an error: const KWARGS_KEY_IDENT_ERROR_CODE: &str = "CF04";
const EXTRA_PARAMETER_ERROR_CODE: &str = "CF05";

#[derive(Debug, Clone)]
pub enum FunctionError {
    NotEnoughParameter {
        missing: String,
        function_type: FunctionType,
        signature: Vec<FunctionParameter>,
    },
    ArgsValueIsNotString,
    ArgsArrayIsNotIterable,
    KWArgsDictIsNotMappable,
    ExtraParameter,
}

impl Into<RuntimeError> for FunctionError {
    fn into(self) -> RuntimeError {
        RuntimeError {
            code: match self {
                FunctionError::NotEnoughParameter { .. } => NOT_ENOUGH_PARAMS_ERROR_CODE,
                FunctionError::ArgsValueIsNotString => WRONG_ARGS_IDENT_ERROR_CODE,
                FunctionError::ArgsArrayIsNotIterable => ARGS_NOT_ITERABLE_ERROR_CODE,
                FunctionError::KWArgsDictIsNotMappable => KWARGS_NOT_MAPPABLE_ERROR_CODE,
                FunctionError::ExtraParameter => EXTRA_PARAMETER_ERROR_CODE,
            },
            label: match self {
                FunctionError::NotEnoughParameter { .. } => {
                    "Not enough parameters in function call".to_owned()
                }
                FunctionError::ArgsValueIsNotString => "not an identifier for *args".to_owned(),
                FunctionError::ArgsArrayIsNotIterable => "*args is not iterable".to_owned(),
                FunctionError::KWArgsDictIsNotMappable => "**kwargs is not mappable".to_owned(),
                FunctionError::ExtraParameter => "Extraneous parameter in function call".to_owned(),
            },
            message: match self {
                FunctionError::NotEnoughParameter {
                    missing,
                    function_type,
                    signature,
                } => format!(
                    "Missing parameter {} for call to {}",
                    missing.trim_start_matches('$'),
                    repr(&function_type, &signature)
                ),
                FunctionError::ArgsValueIsNotString => {
                    "The argument provided for *args is not an identifier".to_owned()
                }
                FunctionError::ArgsArrayIsNotIterable => {
                    "The argument provided for *args is not iterable".to_owned()
                }
                FunctionError::KWArgsDictIsNotMappable => {
                    "The argument provided for **kwargs is not mappable".to_owned()
                }
                FunctionError::ExtraParameter => {
                    "Extraneous parameter passed to function call".to_owned()
                }
            },
        }
    }
}

impl From<FunctionError> for ValueError {
    fn from(e: FunctionError) -> Self {
        ValueError::Runtime(e.into())
    }
}

impl NativeFunction {
    pub fn new(
        name: String,
        function: fn(&CallStack, TypeValues, Vec<FunctionArg>) -> ValueResult,
        signature: Vec<FunctionParameter>,
    ) -> Value {
        Value::new(NativeFunction {
            function,
            signature,
            function_type: FunctionType::Native(name),
        })
    }
}

impl WrappedMethod {
    pub fn new(self_obj: Value, method: Value) -> Value {
        Value::new(WrappedMethod { method, self_obj })
    }
}

impl FunctionType {
    fn to_str(&self) -> String {
        match self {
            FunctionType::Native(ref name) => name.clone(),
            FunctionType::Def(ref name, ..) => name.clone(),
        }
    }

    fn to_repr(&self) -> String {
        match self {
            FunctionType::Native(ref name) => format!("<native function {}>", name),
            FunctionType::Def(ref name, ref module, ..) => {
                format!("<function {} from {}>", name, module)
            }
        }
    }
}

pub(crate) fn repr(function_type: &FunctionType, signature: &[FunctionParameter]) -> String {
    let v: Vec<String> = signature
        .iter()
        .map(|x| -> String {
            match x {
                FunctionParameter::Normal(ref name) => name.clone(),
                FunctionParameter::Optional(ref name) => format!("?{}", name),
                FunctionParameter::WithDefaultValue(ref name, ref value) => {
                    format!("{} = {}", name, value.to_repr())
                }
                FunctionParameter::ArgsArray(ref name) => format!("*{}", name),
                FunctionParameter::KWArgsDict(ref name) => format!("**{}", name),
            }
        })
        .collect();
    format!("{}({})", function_type.to_repr(), v.join(", "))
}

pub(crate) fn to_str(function_type: &FunctionType, signature: &[FunctionParameter]) -> String {
    let v: Vec<String> = signature
        .iter()
        .map(|x| -> String {
            match x {
                FunctionParameter::Normal(ref name) => name.clone(),
                FunctionParameter::Optional(ref name) => name.clone(),
                FunctionParameter::WithDefaultValue(ref name, ref value) => {
                    format!("{} = {}", name, value.to_repr())
                }
                FunctionParameter::ArgsArray(ref name) => format!("*{}", name),
                FunctionParameter::KWArgsDict(ref name) => format!("**{}", name),
            }
        })
        .collect();
    format!("{}({})", function_type.to_str(), v.join(", "))
}

pub(crate) fn parse_signature(
    signature: &[FunctionParameter],
    function_type: &FunctionType,
    positional: Vec<Value>,
    named: LinkedHashMap<String, Value>,
    args: Option<Value>,
    kwargs: Option<Value>,
) -> Result<Vec<FunctionArg>, ValueError> {
    // First map arguments to a vector
    let mut v = Vec::new();
    // Collect args
    let mut av = positional;
    if let Some(x) = args {
        match x.iter() {
            Ok(y) => av.extend(y.iter()),
            Err(..) => return Err(FunctionError::ArgsArrayIsNotIterable.into()),
        }
    };
    let mut args_iter = av.into_iter();
    // Collect kwargs
    let mut kwargs_dict = named;
    if let Some(x) = kwargs {
        match x.iter() {
            Ok(y) => {
                for n in &y {
                    if n.get_type() == "string" {
                        let k = n.to_str();
                        if let Ok(v) = x.at(n) {
                            kwargs_dict.insert(k, v);
                        } else {
                            return Err(FunctionError::KWArgsDictIsNotMappable.into());
                        }
                    } else {
                        return Err(FunctionError::ArgsValueIsNotString.into());
                    }
                }
            }
            Err(..) => return Err(FunctionError::KWArgsDictIsNotMappable.into()),
        }
    }
    // Now verify signature and transform in a value vector
    for parameter in signature {
        match parameter {
            FunctionParameter::Normal(ref name) => {
                if let Some(x) = args_iter.next() {
                    v.push(FunctionArg::Normal(x))
                } else if let Some(ref r) = kwargs_dict.remove(name) {
                    v.push(FunctionArg::Normal(r.clone()));
                } else {
                    return Err(FunctionError::NotEnoughParameter {
                        missing: name.to_string(),
                        function_type: function_type.clone(),
                        signature: signature.to_owned(),
                    }
                    .into());
                }
            }
            FunctionParameter::Optional(ref name) => {
                if let Some(x) = args_iter.next() {
                    v.push(FunctionArg::Optional(Some(x)))
                } else if let Some(ref r) = kwargs_dict.remove(name) {
                    v.push(FunctionArg::Optional(Some(r.clone())));
                } else {
                    v.push(FunctionArg::Optional(None));
                }
            }
            FunctionParameter::WithDefaultValue(ref name, ref value) => {
                if let Some(x) = args_iter.next() {
                    v.push(FunctionArg::Normal(x))
                } else if let Some(ref r) = kwargs_dict.remove(name) {
                    v.push(FunctionArg::Normal(r.clone()));
                } else {
                    v.push(FunctionArg::Normal(value.clone()));
                }
            }
            FunctionParameter::ArgsArray(..) => {
                let argv: Vec<Value> = args_iter.clone().collect();
                v.push(FunctionArg::ArgsArray(argv));
                // We do not use last so we keep ownership of the iterator
                while args_iter.next().is_some() {}
            }
            FunctionParameter::KWArgsDict(..) => {
                v.push(FunctionArg::KWArgsDict(mem::replace(
                    &mut kwargs_dict,
                    Default::default(),
                )));
            }
        }
    }
    if args_iter.next().is_some() || !kwargs_dict.is_empty() {
        return Err(FunctionError::ExtraParameter.into());
    }
    Ok(v)
}

/// Define the function type
impl TypedValue for NativeFunction {
    type Holder = Immutable<NativeFunction>;

    fn values_for_descendant_check_and_freeze<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = Value> + 'a> {
        Box::new(iter::empty())
    }

    fn to_str(&self) -> String {
        to_str(&self.function_type, &self.signature)
    }
    fn to_repr(&self) -> String {
        repr(&self.function_type, &self.signature)
    }

    const TYPE: &'static str = "function";

    fn call(
        &self,
        call_stack: &CallStack,
        type_values: TypeValues,
        positional: Vec<Value>,
        named: LinkedHashMap<String, Value>,
        args: Option<Value>,
        kwargs: Option<Value>,
    ) -> ValueResult {
        let v = parse_signature(
            &self.signature,
            &self.function_type,
            positional,
            named,
            args,
            kwargs,
        )?;

        (self.function)(call_stack, type_values, v)
    }
}

impl TypedValue for WrappedMethod {
    type Holder = Immutable<WrappedMethod>;

    fn values_for_descendant_check_and_freeze<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = Value> + 'a> {
        Box::new(vec![self.method.clone(), self.self_obj.clone()].into_iter())
    }

    fn function_id(&self) -> Option<FunctionId> {
        Some(FunctionId(self.method.data_ptr()))
    }

    fn to_str(&self) -> String {
        self.method.to_str()
    }
    fn to_repr(&self) -> String {
        self.method.to_repr()
    }
    const TYPE: &'static str = "function";

    fn call(
        &self,
        call_stack: &CallStack,
        type_values: TypeValues,
        positional: Vec<Value>,
        named: LinkedHashMap<String, Value>,
        args: Option<Value>,
        kwargs: Option<Value>,
    ) -> ValueResult {
        // The only thing that this wrapper does is insert self at the beginning of the positional
        // vector
        let positional: Vec<Value> = Some(self.self_obj.clone())
            .into_iter()
            .chain(positional.into_iter())
            .collect();
        self.method
            .call(call_stack, type_values, positional, named, args, kwargs)
    }
}
