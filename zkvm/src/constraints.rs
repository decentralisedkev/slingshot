//! Constraint system-related types and operations:
//! Commitments, Variables, Expressions and Constraints.

use bulletproofs::{r1cs, r1cs::ConstraintSystem, PedersenGens};
use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar;
use std::iter::FromIterator;
use std::ops::{Add, Neg};
use subtle::{ConditionallySelectable, ConstantTimeEq};

use crate::encoding;
use crate::errors::VMError;
use crate::scalar_witness::ScalarWitness;

/// Variable represents a high-level R1CS variable specified by its
/// Pedersen commitment. In ZkVM variables are actually indices to a list
/// of stored commitments to permit commitment reblinding (see `reblind` instruction).
#[derive(Clone, Debug)]
pub struct Variable {
    /// TBD: maybe do this as a subtype of the Expression
    pub(crate) commitment: Commitment,
}

/// Expression is a linear combination of high-level variables (`var`),
/// low-level variables (`alloc`) and constants.
#[derive(Clone, Debug)]
pub enum Expression {
    /// Represents a constant. Operations on constants produce constants.
    Constant(ScalarWitness),

    /// Linear combination of R1CS variables and constants.
    LinearCombination(Vec<(r1cs::Variable, Scalar)>, Option<ScalarWitness>),
}

/// Constraint is a boolean function of expressions and other constraints.
/// Constraints can be evaluated to true or false. The `verify` instruction
/// enforces that the final composition evaluates to `true` in zero knowledge.
#[derive(Clone, Debug)]
pub enum Constraint {
    /// Equality constraint between two expressions.
    /// Created by `eq` instruction.
    Eq(Expression, Expression),

    /// Conjunction of two constraints: each must evaluate to true.
    /// Created by `and` instruction.
    And(Box<Constraint>, Box<Constraint>),

    /// Disjunction of two constraints: at least one must evaluate to true.
    /// Created by `or` instruction.
    Or(Box<Constraint>, Box<Constraint>),

    /// Negation of a constraint: must be zero to evaluate to true.
    /// Created by 'not' instruction.
    Not(Box<Constraint>),
    // no witness needed as it's normally true/false and we derive it on the fly during processing.
    // this also allows us not to wrap this enum in a struct.
}

/// Commitment is a represention of an _open_ or _closed_ Pedersen commitment.
#[derive(Clone, Debug)]
pub enum Commitment {
    /// Hides a secret value and its blinding factor in the Ristretto point.
    Closed(CompressedRistretto),

    /// Contains the secret value and its blinding factor for the use in the prover’s VM.
    Open(Box<CommitmentWitness>),
}

/// Prover's representation of the commitment secret: witness and blinding factor
#[derive(Clone, Debug)]
pub struct CommitmentWitness {
    value: ScalarWitness,
    blinding: Scalar,
}

impl Constraint {
    /// Generates and adds to R1CS constraints that enforce that the self evaluates to true.
    /// Implements the logic behind `verify` instruction.
    pub fn verify<CS: r1cs::ConstraintSystem>(self, cs: &mut CS) -> Result<(), VMError> {
        cs.specify_randomized_constraints(move |cs| {
            // Flatten the constraint into one expression
            // Note: cloning because we can't move out of captured variable in an `Fn` closure,
            // and `Box<FnOnce>` is not fully supported yet. (We can update when that happens).
            // Cf. https://github.com/dalek-cryptography/bulletproofs/issues/244
            let (expr, _) = self.clone().flatten(cs)?;

            // Add the resulting expression to the constraint system
            cs.constrain(expr);

            Ok(())
        })
        .map_err(|e| VMError::R1CSError(e))
    }

    fn flatten<CS: r1cs::RandomizedConstraintSystem>(
        self,
        cs: &mut CS,
    ) -> Result<(r1cs::LinearCombination, Option<Scalar>), r1cs::R1CSError> {
        match self {
            Constraint::Eq(expr1, expr2) => {
                let assignment = expr1
                    .eval()
                    .and_then(|x| expr2.eval().map(|y| (x - y).to_scalar()));
                Ok((expr1.to_r1cs_lc() - expr2.to_r1cs_lc(), assignment))
            }
            Constraint::And(c1, c2) => {
                let (a, a_assg) = c1.flatten(cs)?;
                let (b, b_assg) = c2.flatten(cs)?;
                let z = cs.challenge_scalar(b"ZkVM.verify.and-challenge");
                let assignment = a_assg.and_then(|a| b_assg.map(|b| a + z * b));
                Ok((a + z * b, assignment))
            }
            Constraint::Or(c1, c2) => {
                let (a, a_assg) = c1.flatten(cs)?;
                let (b, b_assg) = c2.flatten(cs)?;
                // output expression: a * b
                let (_l, _r, o) = cs.multiply(a, b);
                let assignment = a_assg.and_then(|a| b_assg.map(|b| a * b));
                Ok((r1cs::LinearCombination::from(o), assignment))
            }
            Constraint::Not(c1) => {
                // Compute the input linear combination and its secret assignment
                let (x_lc, x_assg) = c1.flatten(cs)?;

                // Compute assignments for all the wires
                let (xy_assg, xw_assg, y_assg) = match x_assg {
                    Some(x) => {
                        let is_zero = x.ct_eq(&Scalar::zero());
                        let y = Scalar::conditional_select(
                            &Scalar::zero(),
                            &Scalar::one(),
                            is_zero.into(),
                        );
                        let w = Scalar::conditional_select(&x, &Scalar::one(), is_zero.into());
                        let w = w.invert();
                        (Some((x, y)), Some((x, w)), Some(y))
                    }
                    None => (None, None, None),
                };

                // Allocate two multipliers.
                let (l1, r1, o1) = cs.allocate_multiplier(xy_assg)?;
                let (l2, _r2, o2) = cs.allocate_multiplier(xw_assg)?;

                // Add 4 constraints.

                // (1) `x == l1`
                cs.constrain(l1 - x_lc);

                // (2) `l1 == l2` (== x)
                cs.constrain(l1 - l2);

                // (3) `x*y == 0` which implies that y == 0 if x != 0.
                cs.constrain(o1.into());

                // (4) `x*w == 1 - y` which implies that y == 1 if x == 0.
                cs.constrain(o2 - Scalar::one() + r1);

                // Note: w (r2) is left unconstrained — it is a free variable.

                Ok((r1cs::LinearCombination::from(r1), y_assg))
            }
        }
    }
}

impl Commitment {
    /// Returns the number of bytes needed to serialize the Commitment.
    pub fn serialized_length(&self) -> usize {
        32
    }

    /// Converts a Commitment to a compressed point.
    pub fn to_point(&self) -> CompressedRistretto {
        match self {
            Commitment::Closed(x) => *x,
            Commitment::Open(w) => w.to_point(),
        }
    }

    /// Encodes the commitment as a point.
    pub(crate) fn encode(&self, buf: &mut Vec<u8>) {
        encoding::write_point(&self.to_point(), buf);
    }

    /// Creates an open commitment with a zero blinding factor.
    pub fn unblinded<T: Into<ScalarWitness>>(x: T) -> Self {
        Commitment::Open(Box::new(CommitmentWitness {
            blinding: Scalar::zero(),
            value: x.into(),
        }))
    }

    /// Creates an open commitment with a random blinding factor.
    pub fn blinded<T: Into<ScalarWitness>>(x: T) -> Self {
        Commitment::Open(Box::new(CommitmentWitness {
            blinding: Scalar::random(&mut rand::thread_rng()),
            value: x.into(),
        }))
    }

    /// Creates an open commitment with a specified blinding factor.
    pub fn blinded_with_factor<T: Into<ScalarWitness>>(x: T, blinding: Scalar) -> Self {
        Commitment::Open(Box::new(CommitmentWitness {
            blinding,
            value: x.into(),
        }))
    }

    /// Returns a pair of secrets: the committed scalar or integer, and the blinding factor
    /// TBD: rename to `to_option()`.
    pub fn witness(&self) -> Option<(ScalarWitness, Scalar)> {
        match self {
            Commitment::Closed(_) => None,
            Commitment::Open(w) => Some((w.value, w.blinding)),
        }
    }

    /// Returns the committed scalar or integer, without the blinding factor.
    /// If the witness is missing, returns None.
    pub fn assignment(&self) -> Option<ScalarWitness> {
        match self {
            Commitment::Closed(_) => None,
            Commitment::Open(w) => Some(w.value),
        }
    }
}

impl CommitmentWitness {
    fn to_point(&self) -> CompressedRistretto {
        let gens = PedersenGens::default();
        gens.commit(self.value.into(), self.blinding).compress()
    }
}

impl Expression {
    /// Creates a constant expression for a given integer or scalar.
    pub fn constant<S: Into<ScalarWitness>>(a: S) -> Self {
        Expression::Constant(a.into())
    }

    /// Multiplies two expressions by constraining them to the left/right wires
    /// of a newly allocated R1CS multiplier, and returns
    /// the output wire wrapped in Expression type.
    ///
    /// Note: we can't implement this as a `Mul` trait because we have to pass in a
    /// ConstraintSystem, because the `LinearCombination * LinearCombination` case
    /// requires the creation of a multiplier in the constraint system.
    pub fn multiply<CS: r1cs::ConstraintSystem>(self, rhs: Self, cs: &mut CS) -> Self {
        match (self, rhs) {
            // Constant * Constant
            (Expression::Constant(left), Expression::Constant(right)) => {
                Expression::Constant(left * right)
            }
            // Constant * LinearCombination
            (
                Expression::Constant(l),
                Expression::LinearCombination(mut right_terms, right_assignment),
            ) => {
                // Multiply coefficients in right_terms by l,
                // Multiply assignment in right_assignment by l
                for (_, n) in right_terms.iter_mut() {
                    *n = *n * l.to_scalar();
                }
                Expression::LinearCombination(right_terms, right_assignment.map(|r| r * l))
            }
            // LinearCombination * Constant
            (
                Expression::LinearCombination(mut left_terms, left_assignment),
                Expression::Constant(r),
            ) => {
                // Multiply coefficients in left_terms by r,
                // Multiply assignment in left_assignment by r
                for (_, n) in left_terms.iter_mut() {
                    *n = *n * r.to_scalar();
                }
                Expression::LinearCombination(left_terms, left_assignment.map(|l| l * r))
            }
            // LinearCombination * LinearCombination
            // Creates a multiplication gate in r1cs
            (
                Expression::LinearCombination(left_terms, left_assignment),
                Expression::LinearCombination(right_terms, right_assignment),
            ) => {
                let (_, _, output_var) = cs.multiply(
                    r1cs::LinearCombination::from_iter(left_terms),
                    r1cs::LinearCombination::from_iter(right_terms),
                );
                let output_assignment = match (left_assignment, right_assignment) {
                    (Some(l), Some(r)) => Some(l * r),
                    (_, _) => None,
                };
                Expression::LinearCombination(vec![(output_var, Scalar::one())], output_assignment)
            }
        }
    }

    pub(crate) fn to_r1cs_lc(&self) -> r1cs::LinearCombination {
        match self {
            Expression::Constant(a) => a.to_scalar().into(),
            Expression::LinearCombination(terms, _) => r1cs::LinearCombination::from_iter(terms),
        }
    }

    fn eval(&self) -> Option<ScalarWitness> {
        match self {
            Expression::Constant(a) => Some(*a),
            Expression::LinearCombination(_, a) => match a {
                Some(a) => Some(*a),
                None => None,
            },
        }
    }
}

impl Neg for Expression {
    type Output = Expression;

    fn neg(self) -> Expression {
        match self {
            Expression::Constant(a) => Expression::Constant(-a),
            Expression::LinearCombination(mut terms, assignment) => {
                for (_, n) in terms.iter_mut() {
                    *n = -*n;
                }
                Expression::LinearCombination(terms, assignment.map(|a| -a))
            }
        }
    }
}

impl Add for Expression {
    type Output = Expression;

    fn add(self, rhs: Expression) -> Expression {
        match (self, rhs) {
            (Expression::Constant(left), Expression::Constant(right)) => {
                Expression::Constant(left + right)
            }
            (
                Expression::Constant(l),
                Expression::LinearCombination(mut right_terms, right_assignment),
            ) => {
                // prepend constant term to `term vector` in non-constant expression
                right_terms.insert(0, (r1cs::Variable::One(), l.into()));
                Expression::LinearCombination(right_terms, right_assignment.map(|r| l + r))
            }
            (
                Expression::LinearCombination(mut left_terms, left_assignment),
                Expression::Constant(r),
            ) => {
                // append constant term to term vector in non-constant expression
                left_terms.push((r1cs::Variable::One(), r.into()));
                Expression::LinearCombination(left_terms, left_assignment.map(|l| l + r))
            }
            (
                Expression::LinearCombination(mut left_terms, left_assignment),
                Expression::LinearCombination(right_terms, right_assignment),
            ) => {
                // append right terms to left terms in non-constant expression
                left_terms.extend(right_terms);
                Expression::LinearCombination(
                    left_terms,
                    left_assignment.and_then(|l| right_assignment.map(|r| l + r)),
                )
            }
        }
    }
}

// Upcasting witness/points into Commitment

impl From<CommitmentWitness> for Commitment {
    fn from(x: CommitmentWitness) -> Self {
        Commitment::Open(Box::new(x))
    }
}

impl From<CompressedRistretto> for Commitment {
    fn from(x: CompressedRistretto) -> Self {
        Commitment::Closed(x)
    }
}

impl Into<CompressedRistretto> for Commitment {
    fn into(self) -> CompressedRistretto {
        self.to_point()
    }
}
