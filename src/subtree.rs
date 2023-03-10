use ark_ff::{batch_inversion, FftField};
use ark_poly::{
    univariate::DensePolynomial, EvaluationDomain, GeneralEvaluationDomain, Polynomial,
    UVPolynomial,
};

pub use crate::error::Error;
use crate::{fast_eval::FastEval, PolyProcessor};

/// Saves one degree of 2 for FFT when a, b are monic polynomials in leading coefficient
/// panics if a or b are not monic and degree 2
pub fn multiply_pow2_monic_polys<F: FftField>(
    a: &DensePolynomial<F>,
    b: &DensePolynomial<F>,
) -> DensePolynomial<F> {
    let deg_a = a.degree();
    let deg_b = b.degree();

    if deg_a != deg_b {
        panic!("deg_a != deg_b, {}, {}", deg_a, deg_b);
    }

    let monic_deg = deg_a;

    if monic_deg & (monic_deg - 1) != 0 {
        panic!("Poly a is not degree of 2");
    }

    // it's safe to unwrap since for degree 0 previous check would panic for overflow
    if *a.coeffs.last().unwrap() != F::one() {
        panic!("Poly a is not monic");
    }

    if *b.coeffs.last().unwrap() != F::one() {
        panic!("Poly b is not monic");
    }

    // it's safe to unwrap since monic_deg is pow2
    let domain = GeneralEvaluationDomain::<F>::new(2 * monic_deg).unwrap();

    let a_evals = domain.fft(a);
    let b_evals = domain.fft(b);

    let product_evals: Vec<F> = a_evals
        .iter()
        .zip(b_evals.iter())
        .map(|(&a, &b)| a * b)
        .collect();

    /*
        We know that coefficient of x^(2^m) will be 1 so it will end up in front of x^0,
        That's why we just subtract 1 from free coefficient of resulting poly
    */
    let mut product_poly = DensePolynomial::from_coefficients_slice(&domain.ifft(&product_evals));
    product_poly[0] -= F::one();
    product_poly.coeffs.push(F::one());

    product_poly
}

pub struct Pow2ProductSubtree<F: FftField> {
    pub(crate) layers: Vec<Vec<DensePolynomial<F>>>,
    pub(crate) ri: Vec<F>, // ri = 1/zH'(w^i)
}

impl<F: FftField> Pow2ProductSubtree<F> {
    pub fn construct(roots: &[F]) -> Result<Self, Error> {
        let n = roots.len();

        if n == 0 {
            return Err(Error::EmptyRoots);
        }

        if n & (n - 1) != 0 {
            return Err(Error::NotPow2);
        }

        let k: usize = n.trailing_zeros().try_into().unwrap();
        let mut layers = vec![vec![]; k + 1];

        layers[0] = Vec::with_capacity(n);
        for &root in roots {
            let root_monomial = DensePolynomial::from_coefficients_slice(&[-root, F::one()]);
            layers[0].push(root_monomial);
        }

        let mut nodes_on_layer = n;
        for i in 1..=k {
            nodes_on_layer /= 2;
            layers[i] = Vec::with_capacity(nodes_on_layer);
            for j in 0..nodes_on_layer {
                let lhs_node = layers[i - 1][2 * j].clone();
                let rhs_node = layers[i - 1][2 * j + 1].clone();

                layers[i].push(multiply_pow2_monic_polys(&lhs_node, &rhs_node));
            }
        }

        let evals = vec![F::one(); n];
        let vanishing_derivative =
            FastEval::multiply_up_the_tree(&layers, (0, evals.len() - 1), (k, 0), &evals);

        let mut ri = FastEval::divide_down_the_tree(&layers, n, (k, 0), &vanishing_derivative);
        batch_inversion(&mut ri);

        Ok(Self { layers, ri })
    }
}

impl<F: FftField> PolyProcessor<F> for Pow2ProductSubtree<F> {
    fn get_vanishing(&self) -> DensePolynomial<F> {
        let k = self.layers.len() - 1;
        self.layers[k][0].clone()
    }

    fn get_ri(&self) -> Vec<F> {
        self.ri.clone()
    }

    fn evaluate_over_domain(&self, f: &DensePolynomial<F>) -> Vec<F> {
        let n = self.layers[0].len();
        let k = self.layers.len() - 1;

        assert!(f.degree() < n);
        FastEval::divide_down_the_tree(&self.layers, n, (k, 0), f)
    }

    fn interpolate(&self, evals: &[F]) -> DensePolynomial<F> {
        assert_eq!(evals.len(), self.ri.len());
        let k = self.layers.len() - 1;
        let evals = evals
            .iter()
            .zip(self.ri.iter())
            .map(|(&vi, &ri)| vi * ri)
            .collect::<Vec<_>>();
        FastEval::multiply_up_the_tree(&self.layers, (0, evals.len() - 1), (k, 0), &evals)
    }

    fn batch_evaluate_lagrange_basis(&self, point: &F) -> Vec<F> {
        let mut monomials_evals = Vec::with_capacity(self.layers[0].len());
        for root_monomial in &self.layers[0] {
            monomials_evals.push(root_monomial.evaluate(point));
        }
        batch_inversion(&mut monomials_evals);

        let k = self.layers.len() - 1;
        let vh_eval = self.layers[k][0].evaluate(point);

        self.ri
            .iter()
            .zip(monomials_evals.iter())
            .map(|(&ri, monomial_i)| ri * monomial_i * vh_eval)
            .collect()
    }
}

#[cfg(test)]
mod subtree_tests {
    use ark_bn254::Fr;
    use ark_ff::{FftField, One, UniformRand};
    use ark_poly::{univariate::DensePolynomial, Polynomial, UVPolynomial};
    use ark_std::test_rng;

    use crate::{
        subtree::{multiply_pow2_monic_polys, Pow2ProductSubtree},
        PolyProcessor,
    };

    /// given x coords construct Li polynomials
    fn construct_lagrange_basis<F: FftField>(evaluation_domain: &[F]) -> Vec<DensePolynomial<F>> {
        let mut bases = Vec::with_capacity(evaluation_domain.len());
        for i in 0..evaluation_domain.len() {
            let mut l_i = DensePolynomial::from_coefficients_slice(&[F::one()]);
            let x_i = evaluation_domain[i];
            for (j, _) in evaluation_domain.iter().enumerate() {
                if j != i {
                    let xi_minus_xj_inv = (x_i - evaluation_domain[j]).inverse().unwrap();
                    l_i = &l_i
                        * &DensePolynomial::from_coefficients_slice(&[
                            -evaluation_domain[j] * xi_minus_xj_inv,
                            xi_minus_xj_inv,
                        ]);
                }
            }

            bases.push(l_i);
        }

        bases
    }

    #[test]
    fn test_monic_fft() {
        let n = 32;
        let mut rng = test_rng();

        let mut a = DensePolynomial::<Fr>::rand(n, &mut rng);
        a.coeffs[n] = Fr::one();

        let mut b = DensePolynomial::<Fr>::rand(n, &mut rng);
        b.coeffs[n] = Fr::one();

        let product_slow = &a * &b;
        let product_fast = multiply_pow2_monic_polys(&a, &b);
        assert_eq!(product_fast, product_slow);
    }

    #[test]
    fn test_tree_construction() {
        let n: usize = 32;
        let k: usize = n.trailing_zeros().try_into().unwrap();
        let mut rng = test_rng();

        let roots: Vec<_> = (0..n).map(|_| Fr::rand(&mut rng)).collect();
        let subtree = Pow2ProductSubtree::construct(&roots).unwrap();

        let lagrange_basis = construct_lagrange_basis(&roots);

        let mut vanishing = DensePolynomial::from_coefficients_slice(&[Fr::one()]);
        for root in roots {
            vanishing = &vanishing * &DensePolynomial::from_coefficients_slice(&[-root, Fr::one()]);
        }

        assert_eq!(subtree.layers[k][0], vanishing);

        let alpha = Fr::rand(&mut rng);
        let li_evals_slow: Vec<_> = lagrange_basis
            .iter()
            .map(|li| li.evaluate(&alpha))
            .collect();

        let li_evals_fast = subtree.batch_evaluate_lagrange_basis(&alpha);
        assert_eq!(li_evals_slow, li_evals_fast);
    }

    #[test]
    fn test_interpolation() {
        let n: usize = 32;
        let mut rng = test_rng();

        let roots: Vec<_> = (0..n).map(|_| Fr::rand(&mut rng)).collect();
        let subtree = Pow2ProductSubtree::construct(&roots).unwrap();

        let lagrange_basis = construct_lagrange_basis(&roots);
        let f_evals: Vec<_> = (0..n).map(|_| Fr::rand(&mut rng)).collect();

        let mut f_slow = DensePolynomial::default();
        for (li, &fi) in lagrange_basis.iter().zip(f_evals.iter()) {
            f_slow += (fi, li);
        }

        let f_fast = subtree.interpolate(&f_evals);
        assert_eq!(f_slow, f_fast);
    }

    #[test]
    fn test_evaluate_over_domain() {
        let n: usize = 32;
        let mut rng = test_rng();

        let roots: Vec<_> = (0..n).map(|_| Fr::rand(&mut rng)).collect();
        let subtree = Pow2ProductSubtree::construct(&roots).unwrap();

        let lagrange_basis = construct_lagrange_basis(&roots);
        let f_evals: Vec<_> = (0..n).map(|_| Fr::rand(&mut rng)).collect();

        let mut f = DensePolynomial::default();
        for (li, &fi) in lagrange_basis.iter().zip(f_evals.iter()) {
            f += (fi, li);
        }

        let f_computed_evals = subtree.evaluate_over_domain(&f);
        assert_eq!(f_evals, f_computed_evals);
    }
}
