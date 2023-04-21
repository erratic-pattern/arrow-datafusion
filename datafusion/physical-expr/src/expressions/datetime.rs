// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use crate::intervals::cp_solver::{propagate_arithmetic, propagate_comparison};
use crate::intervals::{apply_operator, Interval};
use crate::physical_expr::down_cast_any_ref;
use crate::PhysicalExpr;
use arrow::array::{Array, ArrayRef};
use arrow::compute::try_unary;
use arrow::datatypes::{DataType, Date32Type, Date64Type, Schema};
use arrow::record_batch::RecordBatch;

use datafusion_common::cast::*;
use datafusion_common::scalar::*;
use datafusion_common::Result;
use datafusion_common::{DataFusionError, ScalarValue};
use datafusion_expr::type_coercion::binary::coerce_types;
use datafusion_expr::{ColumnarValue, Operator};
use std::any::Any;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use super::binary::{
    interval_array_op, interval_scalar_interval_op, ts_array_op, ts_interval_array_op,
    ts_scalar_interval_op, ts_scalar_ts_op,
};

/// Perform DATE/TIME/TIMESTAMP +/ INTERVAL math
#[derive(Debug)]
pub struct DateTimeIntervalExpr {
    lhs: Arc<dyn PhysicalExpr>,
    op: Operator,
    rhs: Arc<dyn PhysicalExpr>,
    // TODO: move type checking to the planning phase and not in the physical expr
    // so we can remove this
    input_schema: Schema,
}

impl DateTimeIntervalExpr {
    /// Create a new instance of DateIntervalExpr
    pub fn try_new(
        lhs: Arc<dyn PhysicalExpr>,
        op: Operator,
        rhs: Arc<dyn PhysicalExpr>,
        input_schema: &Schema,
    ) -> Result<Self> {
        match (
            lhs.data_type(input_schema)?,
            op,
            rhs.data_type(input_schema)?,
        ) {
            (
                DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _),
                Operator::Plus | Operator::Minus,
                DataType::Interval(_),
            )
            | (DataType::Timestamp(_, _), Operator::Minus, DataType::Timestamp(_, _))
            | (DataType::Interval(_), Operator::Plus, DataType::Timestamp(_, _))
            | (
                DataType::Interval(_),
                Operator::Plus | Operator::Minus,
                DataType::Interval(_),
            ) => Ok(Self {
                lhs,
                op,
                rhs,
                input_schema: input_schema.clone(),
            }),
            (lhs, _, rhs) => Err(DataFusionError::Execution(format!(
                "Invalid operation {op} between '{lhs}' and '{rhs}' for DateIntervalExpr"
            ))),
        }
    }

    /// Get the left-hand side expression
    pub fn lhs(&self) -> &Arc<dyn PhysicalExpr> {
        &self.lhs
    }

    /// Get the operator
    pub fn op(&self) -> &Operator {
        &self.op
    }

    /// Get the right-hand side expression
    pub fn rhs(&self) -> &Arc<dyn PhysicalExpr> {
        &self.rhs
    }
}

impl Display for DateTimeIntervalExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} {}", self.lhs, self.op, self.rhs)
    }
}

impl PhysicalExpr for DateTimeIntervalExpr {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self, input_schema: &Schema) -> Result<DataType> {
        coerce_types(
            &self.lhs.data_type(input_schema)?,
            &Operator::Minus,
            &self.rhs.data_type(input_schema)?,
        )
    }

    fn nullable(&self, input_schema: &Schema) -> Result<bool> {
        self.lhs.nullable(input_schema)
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let lhs_value = self.lhs.evaluate(batch)?;
        let rhs_value = self.rhs.evaluate(batch)?;
        // Invert sign for subtraction
        let sign = match self.op {
            Operator::Plus => 1,
            Operator::Minus => -1,
            _ => {
                // this should be unreachable because we check the operators in `try_new`
                let msg = "Invalid operator for DateIntervalExpr";
                return Err(DataFusionError::Internal(msg.to_string()));
            }
        };
        // RHS is first checked. If it is a Scalar, there are 2 options:
        // Either LHS is also a Scalar and matching operation is applied,
        // or LHS is an Array and unary operations for related types are
        // applied in evaluate_array function. If RHS is an Array, then
        // LHS must also be, moreover; they must be the same Timestamp type.
        match (lhs_value, rhs_value) {
            (ColumnarValue::Scalar(operand_lhs), ColumnarValue::Scalar(operand_rhs)) => {
                Ok(ColumnarValue::Scalar(if sign > 0 {
                    operand_lhs.add(&operand_rhs)?
                } else {
                    operand_lhs.sub(&operand_rhs)?
                }))
            }
            (ColumnarValue::Array(array_lhs), ColumnarValue::Scalar(operand_rhs)) => {
                evaluate_temporal_array(array_lhs, sign, &operand_rhs)
            }

            (ColumnarValue::Array(array_lhs), ColumnarValue::Array(array_rhs)) => {
                evaluate_temporal_arrays(&array_lhs, sign, &array_rhs)
            }
            (_, _) => {
                let msg = "If RHS of the operation is an array, then LHS also must be";
                Err(DataFusionError::Internal(msg.to_string()))
            }
        }
    }

    fn evaluate_bounds(&self, children: &[&Interval]) -> Result<Interval> {
        // Get children intervals:
        let left_interval = children[0];
        let right_interval = children[1];
        // Calculate current node's interval:
        apply_operator(&self.op, left_interval, right_interval)
    }

    fn propagate_constraints(
        &self,
        interval: &Interval,
        children: &[&Interval],
    ) -> Result<Vec<Option<Interval>>> {
        // Get children intervals. Graph brings
        let left_interval = children[0];
        let right_interval = children[1];
        let (left, right) = if self.op.is_comparison_operator() {
            if interval == &Interval::CERTAINLY_FALSE {
                // TODO: We will handle strictly false clauses by negating
                //       the comparison operator (e.g. GT to LE, LT to GE)
                //       once open/closed intervals are supported.
                return Ok(vec![]);
            }
            // Propagate the comparison operator.
            propagate_comparison(&self.op, left_interval, right_interval)?
        } else {
            // Propagate the arithmetic operator.
            propagate_arithmetic(&self.op, interval, left_interval, right_interval)?
        };
        Ok(vec![left, right])
    }

    fn children(&self) -> Vec<Arc<dyn PhysicalExpr>> {
        vec![self.lhs.clone(), self.rhs.clone()]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn PhysicalExpr>> {
        Ok(Arc::new(DateTimeIntervalExpr::try_new(
            children[0].clone(),
            self.op,
            children[1].clone(),
            &self.input_schema,
        )?))
    }
}

impl PartialEq<dyn Any> for DateTimeIntervalExpr {
    fn eq(&self, other: &dyn Any) -> bool {
        down_cast_any_ref(other)
            .downcast_ref::<Self>()
            .map(|x| self.lhs.eq(&x.lhs) && self.op == x.op && self.rhs.eq(&x.rhs))
            .unwrap_or(false)
    }
}

pub fn evaluate_temporal_array(
    array: ArrayRef,
    sign: i32,
    scalar: &ScalarValue,
) -> Result<ColumnarValue> {
    match (array.data_type(), scalar.get_datatype()) {
        // Date +- Interval
        (DataType::Date32, DataType::Interval(_)) => {
            let array = as_date32_array(&array)?;
            let ret = Arc::new(try_unary::<Date32Type, _, Date32Type>(array, |days| {
                Ok(date32_add(days, scalar, sign)?)
            })?) as ArrayRef;
            Ok(ColumnarValue::Array(ret))
        }
        (DataType::Date64, DataType::Interval(_)) => {
            let array = as_date64_array(&array)?;
            let ret = Arc::new(try_unary::<Date64Type, _, Date64Type>(array, |ms| {
                Ok(date64_add(ms, scalar, sign)?)
            })?) as ArrayRef;
            Ok(ColumnarValue::Array(ret))
        }
        // Timestamp - Timestamp
        (DataType::Timestamp(_, _), DataType::Timestamp(_, _)) if sign == -1 => {
            ts_scalar_ts_op(array, scalar)
        }
        // Interval +- Interval
        (DataType::Interval(_), DataType::Interval(_)) => {
            interval_scalar_interval_op(array, sign, scalar)
        }
        // Timestamp +- Interval
        (DataType::Timestamp(_, _), DataType::Interval(_)) => {
            ts_scalar_interval_op(array, sign, scalar)
        }
        (_, _) => Err(DataFusionError::Execution(format!(
            "Invalid lhs type for DateIntervalExpr: {}",
            array.data_type()
        )))?,
    }
}

// This function evaluates temporal array operations, such as timestamp - timestamp, interval + interval,
// timestamp + interval, and interval + timestamp. It takes two arrays as input and an integer sign representing
// the operation (+1 for addition and -1 for subtraction). It returns a ColumnarValue as output, which can hold
// either a scalar or an array.
pub fn evaluate_temporal_arrays(
    array_lhs: &ArrayRef,
    sign: i32,
    array_rhs: &ArrayRef,
) -> Result<ColumnarValue> {
    let ret = match (array_lhs.data_type(), array_rhs.data_type()) {
        // Timestamp - Timestamp operations, operands of only the same types are supported.
        (DataType::Timestamp(_, _), DataType::Timestamp(_, _)) => {
            ts_array_op(array_lhs, array_rhs)?
        }
        // Interval (+ , -) Interval operations
        (DataType::Interval(_), DataType::Interval(_)) => {
            interval_array_op(array_lhs, array_rhs, sign)?
        }
        // Timestamp (+ , -) Interval and Interval + Timestamp operations
        // Interval - Timestamp operation is not rational hence not supported
        (DataType::Timestamp(_, _), DataType::Interval(_)) => {
            ts_interval_array_op(array_lhs, sign, array_rhs)?
        }
        (DataType::Interval(_), DataType::Timestamp(_, _)) if sign == 1 => {
            ts_interval_array_op(array_rhs, sign, array_lhs)?
        }
        (_, _) => Err(DataFusionError::Execution(format!(
            "Invalid array types for DateIntervalExpr: {} {} {}",
            array_lhs.data_type(),
            sign,
            array_rhs.data_type()
        )))?,
    };
    Ok(ColumnarValue::Array(ret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create_physical_expr;
    use crate::execution_props::ExecutionProps;
    use arrow::array::{ArrayRef, Date32Builder};
    use arrow::datatypes::*;
    use arrow_array::IntervalMonthDayNanoArray;
    use chrono::{Duration, NaiveDate};
    use datafusion_common::delta::shift_months;
    use datafusion_common::{Column, Result, ToDFSchema};
    use datafusion_expr::Expr;
    use std::ops::Add;

    #[test]
    fn add_11_months() {
        let prior = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let actual = shift_months(prior, 11);
        assert_eq!(format!("{actual:?}").as_str(), "2000-12-01");
    }

    #[test]
    fn add_12_months() {
        let prior = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let actual = shift_months(prior, 12);
        assert_eq!(format!("{actual:?}").as_str(), "2001-01-01");
    }

    #[test]
    fn add_13_months() {
        let prior = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let actual = shift_months(prior, 13);
        assert_eq!(format!("{actual:?}").as_str(), "2001-02-01");
    }

    #[test]
    fn sub_11_months() {
        let prior = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let actual = shift_months(prior, -11);
        assert_eq!(format!("{actual:?}").as_str(), "1999-02-01");
    }

    #[test]
    fn sub_12_months() {
        let prior = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let actual = shift_months(prior, -12);
        assert_eq!(format!("{actual:?}").as_str(), "1999-01-01");
    }

    #[test]
    fn sub_13_months() {
        let prior = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
        let actual = shift_months(prior, -13);
        assert_eq!(format!("{actual:?}").as_str(), "1998-12-01");
    }

    #[test]
    fn add_32_day_time() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date32(Some(0)));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::new_interval_dt(1, 0));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::Date32(Some(d))) => {
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let res = epoch.add(Duration::days(d as i64));
                assert_eq!(format!("{res:?}").as_str(), "1970-01-02");
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }

        Ok(())
    }

    #[test]
    fn sub_32_year_month() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date32(Some(0)));
        let op = Operator::Minus;
        let interval = Expr::Literal(ScalarValue::IntervalYearMonth(Some(13)));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::Date32(Some(d))) => {
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let res = epoch.add(Duration::days(d as i64));
                assert_eq!(format!("{res:?}").as_str(), "1968-12-01");
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }

        Ok(())
    }

    #[test]
    fn add_64_day_time() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date64(Some(0)));
        let op = Operator::Plus;
        let interval =
            Expr::Literal(ScalarValue::new_interval_dt(-15, -24 * 60 * 60 * 1000));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::Date64(Some(d))) => {
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let res = epoch.add(Duration::milliseconds(d));
                assert_eq!(format!("{res:?}").as_str(), "1969-12-16");
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }

        Ok(())
    }

    #[test]
    fn add_32_year_month() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date32(Some(0)));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::IntervalYearMonth(Some(1)));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::Date32(Some(d))) => {
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let res = epoch.add(Duration::days(d as i64));
                assert_eq!(format!("{res:?}").as_str(), "1970-02-01");
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }

        Ok(())
    }

    #[test]
    fn add_32_month_day_nano() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date32(Some(0)));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::new_interval_mdn(-12, -15, -42));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::Date32(Some(d))) => {
                let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let res = epoch.add(Duration::days(d as i64));
                assert_eq!(format!("{res:?}").as_str(), "1968-12-17");
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }

        Ok(())
    }

    #[test]
    fn add_1_millisecond() -> Result<()> {
        // setup
        let now_ts_ns = chrono::Utc::now().timestamp_nanos();
        let dt = Expr::Literal(ScalarValue::TimestampNanosecond(Some(now_ts_ns), None));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::new_interval_dt(0, 1));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::TimestampNanosecond(Some(ts), None)) => {
                assert_eq!(ts, now_ts_ns + 1_000_000);
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }
        Ok(())
    }

    #[test]
    fn add_2_hours() -> Result<()> {
        // setup
        let now_ts_s = chrono::Utc::now().timestamp();
        let dt = Expr::Literal(ScalarValue::TimestampSecond(Some(now_ts_s), None));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::new_interval_dt(0, 2 * 3600 * 1_000));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::TimestampSecond(Some(ts), None)) => {
                assert_eq!(ts, now_ts_s + 2 * 3600);
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }
        Ok(())
    }

    #[test]
    fn sub_4_hours() -> Result<()> {
        // setup
        let now_ts_s = chrono::Utc::now().timestamp();
        let dt = Expr::Literal(ScalarValue::TimestampSecond(Some(now_ts_s), None));
        let op = Operator::Minus;
        let interval = Expr::Literal(ScalarValue::new_interval_dt(0, 4 * 3600 * 1_000));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::TimestampSecond(Some(ts), None)) => {
                assert_eq!(ts, now_ts_s - 4 * 3600);
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }
        Ok(())
    }

    #[test]
    fn add_8_days() -> Result<()> {
        // setup
        let now_ts_ns = chrono::Utc::now().timestamp_nanos();
        let dt = Expr::Literal(ScalarValue::TimestampNanosecond(Some(now_ts_ns), None));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::new_interval_dt(8, 0));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::TimestampNanosecond(Some(ts), None)) => {
                assert_eq!(ts, now_ts_ns + 8 * 86400 * 1_000_000_000);
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }
        Ok(())
    }

    #[test]
    fn sub_16_days() -> Result<()> {
        // setup
        let now_ts_ns = chrono::Utc::now().timestamp_nanos();
        let dt = Expr::Literal(ScalarValue::TimestampNanosecond(Some(now_ts_ns), None));
        let op = Operator::Minus;
        let interval = Expr::Literal(ScalarValue::new_interval_dt(16, 0));

        // exercise
        let res = exercise(&dt, op, &interval)?;

        // assert
        match res {
            ColumnarValue::Scalar(ScalarValue::TimestampNanosecond(Some(ts), None)) => {
                assert_eq!(ts, now_ts_ns - 16 * 86400 * 1_000_000_000);
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }
        Ok(())
    }

    #[test]
    fn array_add_26_days() -> Result<()> {
        let mut builder = Date32Builder::with_capacity(8);
        builder.append_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
        let a: ArrayRef = Arc::new(builder.finish());

        let schema = Schema::new(vec![Field::new("a", DataType::Date32, false)]);
        let batch = RecordBatch::try_new(Arc::new(schema.clone()), vec![a])?;
        let dfs = schema.clone().to_dfschema()?;
        let props = ExecutionProps::new();

        let dt = Expr::Column(Column::from_name("a"));
        let interval = Expr::Literal(ScalarValue::new_interval_dt(26, 0));
        let op = Operator::Plus;

        let lhs = create_physical_expr(&dt, &dfs, &schema, &props)?;
        let rhs = create_physical_expr(&interval, &dfs, &schema, &props)?;

        let cut = DateTimeIntervalExpr::try_new(lhs, op, rhs, &schema)?;
        let res = cut.evaluate(&batch)?;

        let mut builder = Date32Builder::with_capacity(8);
        builder.append_slice(&[26, 27, 28, 29, 30, 31, 32, 33]);
        let expected: ArrayRef = Arc::new(builder.finish());

        // assert
        match res {
            ColumnarValue::Array(array) => {
                assert_eq!(&array, &expected)
            }
            _ => Err(DataFusionError::NotImplemented(
                "Unexpected result!".to_string(),
            ))?,
        }

        Ok(())
    }

    #[test]
    fn invalid_interval() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date32(Some(0)));
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::Null);

        // exercise
        let res = exercise(&dt, op, &interval);
        assert!(res.is_err(), "Can't add a NULL interval");

        Ok(())
    }

    #[test]
    fn invalid_date() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Null);
        let op = Operator::Plus;
        let interval = Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(0)));

        // exercise
        let res = exercise(&dt, op, &interval);
        assert!(res.is_err(), "Can't add to NULL date");

        Ok(())
    }

    #[test]
    fn invalid_op() -> Result<()> {
        // setup
        let dt = Expr::Literal(ScalarValue::Date32(Some(0)));
        let op = Operator::Eq;
        let interval = Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(0)));

        // exercise
        let res = exercise(&dt, op, &interval);
        assert!(res.is_err(), "Can't add dates with == operator");

        Ok(())
    }

    fn exercise(dt: &Expr, op: Operator, interval: &Expr) -> Result<ColumnarValue> {
        let mut builder = Date32Builder::with_capacity(1);
        builder.append_value(0);
        let a: ArrayRef = Arc::new(builder.finish());
        let schema = Schema::new(vec![Field::new("a", DataType::Date32, false)]);
        let batch = RecordBatch::try_new(Arc::new(schema.clone()), vec![a])?;

        let dfs = schema.clone().to_dfschema()?;
        let props = ExecutionProps::new();

        let lhs = create_physical_expr(dt, &dfs, &schema, &props)?;
        let rhs = create_physical_expr(interval, &dfs, &schema, &props)?;

        let lhs_str = format!("{lhs}");
        let rhs_str = format!("{rhs}");

        let cut = DateTimeIntervalExpr::try_new(lhs, op, rhs, &schema)?;

        assert_eq!(lhs_str, format!("{}", cut.lhs()));
        assert_eq!(op, cut.op().clone());
        assert_eq!(rhs_str, format!("{}", cut.rhs()));

        let res = cut.evaluate(&batch)?;
        Ok(res)
    }

    // In this test, ArrayRef of one element arrays is evaluated with some ScalarValues,
    // aiming that evaluate_temporal_array function is working properly and shows the same
    // behavior with ScalarValue arithmetic.
    fn experiment(
        timestamp_scalar: ScalarValue,
        interval_scalar: ScalarValue,
    ) -> Result<()> {
        let timestamp_array = timestamp_scalar.to_array();
        let interval_array = interval_scalar.to_array();

        // timestamp + interval
        if let ColumnarValue::Array(res1) =
            evaluate_temporal_array(timestamp_array.clone(), 1, &interval_scalar)?
        {
            let res2 = timestamp_scalar.add(&interval_scalar)?.to_array();
            assert_eq!(
                &res1, &res2,
                "Timestamp Scalar={} + Interval Scalar={}",
                timestamp_scalar, interval_scalar
            );
        }

        // timestamp - interval
        if let ColumnarValue::Array(res1) =
            evaluate_temporal_array(timestamp_array.clone(), -1, &interval_scalar)?
        {
            let res2 = timestamp_scalar.sub(&interval_scalar)?.to_array();
            assert_eq!(
                &res1, &res2,
                "Timestamp Scalar={} - Interval Scalar={}",
                timestamp_scalar, interval_scalar
            );
        }

        // timestamp - timestamp
        if let ColumnarValue::Array(res1) =
            evaluate_temporal_array(timestamp_array.clone(), -1, &timestamp_scalar)?
        {
            let res2 = timestamp_scalar.sub(&timestamp_scalar)?.to_array();
            assert_eq!(
                &res1, &res2,
                "Timestamp Scalar={} - Timestamp Scalar={}",
                timestamp_scalar, timestamp_scalar
            );
        }

        // interval - interval
        if let ColumnarValue::Array(res1) =
            evaluate_temporal_array(interval_array.clone(), -1, &interval_scalar)?
        {
            let res2 = interval_scalar.sub(&interval_scalar)?.to_array();
            assert_eq!(
                &res1, &res2,
                "Interval Scalar={} - Interval Scalar={}",
                interval_scalar, interval_scalar
            );
        }

        // interval + interval
        if let ColumnarValue::Array(res1) =
            evaluate_temporal_array(interval_array, 1, &interval_scalar)?
        {
            let res2 = interval_scalar.add(&interval_scalar)?.to_array();
            assert_eq!(
                &res1, &res2,
                "Interval Scalar={} + Interval Scalar={}",
                interval_scalar, interval_scalar
            );
        }

        Ok(())
    }
    #[test]
    fn test_evalute_with_scalar() -> Result<()> {
        // Timestamp (sec) & Interval (DayTime)
        let timestamp_scalar = ScalarValue::TimestampSecond(
            Some(
                NaiveDate::from_ymd_opt(2023, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .timestamp(),
            ),
            None,
        );
        let interval_scalar = ScalarValue::new_interval_dt(0, 1_000);

        experiment(timestamp_scalar, interval_scalar)?;

        // Timestamp (millisec) & Interval (DayTime)
        let timestamp_scalar = ScalarValue::TimestampMillisecond(
            Some(
                NaiveDate::from_ymd_opt(2023, 1, 1)
                    .unwrap()
                    .and_hms_milli_opt(0, 0, 0, 0)
                    .unwrap()
                    .timestamp_millis(),
            ),
            None,
        );
        let interval_scalar = ScalarValue::new_interval_dt(0, 1_000);

        experiment(timestamp_scalar, interval_scalar)?;

        // Timestamp (nanosec) & Interval (MonthDayNano)
        let timestamp_scalar = ScalarValue::TimestampNanosecond(
            Some(
                NaiveDate::from_ymd_opt(2023, 1, 1)
                    .unwrap()
                    .and_hms_nano_opt(0, 0, 0, 0)
                    .unwrap()
                    .timestamp_nanos(),
            ),
            None,
        );
        let interval_scalar = ScalarValue::new_interval_mdn(0, 0, 1_000);

        experiment(timestamp_scalar, interval_scalar)?;

        // Timestamp (nanosec) & Interval (MonthDayNano), negatively resulting cases

        let timestamp_scalar = ScalarValue::TimestampNanosecond(
            Some(
                NaiveDate::from_ymd_opt(1970, 1, 1)
                    .unwrap()
                    .and_hms_nano_opt(0, 0, 0, 000)
                    .unwrap()
                    .timestamp_nanos(),
            ),
            None,
        );

        Arc::new(IntervalMonthDayNanoArray::from(vec![1_000])); // 1 us
        let interval_scalar = ScalarValue::new_interval_mdn(0, 0, 1_000);

        experiment(timestamp_scalar, interval_scalar)?;

        // Timestamp (sec) & Interval (YearMonth)
        let timestamp_scalar = ScalarValue::TimestampSecond(
            Some(
                NaiveDate::from_ymd_opt(2023, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .timestamp(),
            ),
            None,
        );
        let interval_scalar = ScalarValue::new_interval_ym(0, 1);

        experiment(timestamp_scalar, interval_scalar)?;

        Ok(())
    }
}