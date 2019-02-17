use std::cell::RefCell;
use std::cmp::Ordering;

use bit_field::BitField;
use integer::Integer;
use num_traits::{One, Zero};
use smallvec::SmallVec;

use crate::algorithms::{
    __add2, __add_scalar, __mul3, __sub2, add2, cmp_slice, is_zero, mac3, mod_inv1, mul_hi, mul_lo,
    sub2, sub2rev,
};
use crate::big_digit::{self, BigDigit, DoubleBigDigit, BITS};
use crate::{BigUint, VEC_SIZE};

/// 2**54
const TWO54FLOAT: f64 = 1.0 * 0x08000000 as f64 * 0x08000000 as f64;

/// 2**96
const TWO96FLOAT: f64 = 0x0001000000000000u64 as f64 * 0x0001000000000000u64 as f64;

lazy_static! {
    static ref ONE: BigUint = One::one();
}

pub fn div(u: &BigUint, d: &BigUint) -> BigUint {
    if d.is_zero() {
        panic!("Cannot divide by zero");
    }
    if u.is_zero() {
        return Zero::zero();
    }
    if d.data.len() == 1 {
        let mut q = u.clone();

        if d.data[0] == 1 {
            return q;
        }

        div_digit(&mut q, d.data[0]);
        return q;
    }

    // Required or the q_len calculation below can underflow:
    match u.cmp(d) {
        Ordering::Less => return Zero::zero(),
        Ordering::Equal => return One::one(),
        Ordering::Greater => {} // Do nothing
    }

    div_mont(u, d)
}

pub fn rem(u: &BigUint, d: &BigUint) -> BigUint {
    if d.is_zero() {
        panic!("Cannot divide by zero");
    }
    if u.is_zero() {
        return Zero::zero();
    }
    if d.data.len() == 1 {
        if d.data[0] == 1 {
            return Zero::zero();
        }

        return rem_digit(&u, d.data[0]).into();
    }

    // Required or the q_len calculation below can underflow:
    match u.cmp(d) {
        Ordering::Less => return u.clone(),
        Ordering::Equal => return Zero::zero(),
        Ordering::Greater => {} // Do nothing
    }

    rem_mont(u, d)
}

pub fn div_rem(u: &BigUint, d: &BigUint) -> (BigUint, BigUint) {
    if d.is_zero() {
        panic!("Cannot divide by zero");
    }
    if u.is_zero() {
        return (Zero::zero(), Zero::zero());
    }
    if d.data.len() == 1 {
        let mut q = u.clone();

        if d.data[0] == 1 {
            return (q, Zero::zero());
        }

        let rem = div_rem_digit(&mut q, d.data[0]);
        return (q, rem.into());
    }

    // Required or the q_len calculation below can underflow:
    match u.cmp(d) {
        Ordering::Less => return (Zero::zero(), u.clone()),
        Ordering::Equal => return (One::one(), Zero::zero()),
        Ordering::Greater => {} // Do nothing
    }

    div_rem_mont(u, d)
}

#[inline]
fn div_rem_small(x: &BigUint, y: &BigUint, fquo: f64) -> (BigUint, BigUint) {
    let ncmax = 16;

    let mut itmp = fquo as BigDigit;
    let yinv = y * &itmp;
    let mut r = x.clone();
    let bw = __sub2(&mut r.data, &yinv.data);
    r.normalize();

    // If `bw == true` we need an additional correction, `+= y`.
    // nc tracks the number of correction steps needed
    let mut nc = 0;
    if bw {
        while (nc < ncmax) && y < &r {
            nc += 1;
            __add2(&mut r.data, &y.data);
            r.normalize();

            // decr quotient by 1 to account for extra add-y
            itmp -= 1;
        }
    } else {
        while (nc < ncmax) && &r > y {
            nc += 1;
            __sub2(&mut r.data, &y.data);
            r.normalize();

            // inc quotient by 1 for extra sub-y
            itmp += 1;
        }
    }

    debug_assert!(nc < ncmax);
    debug_assert!(&r < y, "remainder should be less than the modulus");

    (itmp.into(), r.into())
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::default();
}

#[derive(Debug, Default)]
struct Cache {
    /// Last quotient used. WARNING: need to externalize this to be safe against secret leaks.
    w: BigUint,
    /// Last quotient inverse.
    winv: BigUint,
    /// Last base power used.
    basepow: BigUint,

    // Scratch memory
    scratch: Vec<BigDigit>,
    cy: BigUint,
    tmp: Vec<BigDigit>,
}

fn div_mont(x: &BigUint, y: &BigUint) -> BigUint {
    div_rem_mont_inner(x, y, true, false).0.unwrap()
}

fn rem_mont(x: &BigUint, y: &BigUint) -> BigUint {
    div_rem_mont_inner(x, y, false, true).1.unwrap()
}

fn div_rem_mont(x: &BigUint, y: &BigUint) -> (BigUint, BigUint) {
    let res = div_rem_mont_inner(x, y, true, true);
    (res.0.unwrap(), res.1.unwrap())
}

fn div_rem_mont_inner(
    x: &BigUint,
    y: &BigUint,
    with_div: bool,
    with_rem: bool,
) -> (Option<BigUint>, Option<BigUint>) {
    // println!("div_rem_mont {} / {}", x, y);
    let len_x = x.data.len();
    if len_x == y.data.len() {
        // First attempt at fast-mod based on FP approximation of the quotient `x/y`,
        // which switches to binary for `x/y > 2**53`.

        let (lo, i) = get_leading_bits(x);
        let (itmp, j) = get_leading_bits(y);
        let bw = 1u64.wrapping_shl(i.wrapping_sub(j) as u32);

        // Power-of-2 sacaling needed on ratio of left justified leading bits.
        // TODO: Use FloatBigDigit
        let fquo = (bw as f64) * (lo as f64 / itmp as f64);

        let (q, r) = if fquo < TWO54FLOAT {
            div_rem_small(x, y, fquo)
        } else {
            div_rem_knuth(x, y)
        };

        return match (with_div, with_rem) {
            (true, true) => (Some(q), Some(r)),
            (true, false) => (Some(q), None),
            (false, true) => (None, Some(r)),
            (false, false) => (None, None),
        };
    }

    // Scratch space
    CACHE.with(|c| {
        let cache = &mut *c.borrow_mut();

        let scratch = &mut cache.scratch;
        let cy = &mut cache.cy;
        let tmp = &mut cache.tmp;

        if scratch.len() < 4 * len_x {
            scratch.resize(4 * len_x, 0);
        }

        // Modulus must be odd for Montgomery.
        let nshift = y.trailing_zeros() as usize;

        let x_adj: BigUint;
        let y_adj: BigUint;
        let mut rem_save = BigUint::zero();

        let (v, w) = if nshift > 0 {
            let nws = (nshift + big_digit::BITS - 1) / big_digit::BITS;
            let nbs = nshift & (big_digit::BITS - 1);

            // Save the bottom nshifts of x
            let mask = (-1i64 as BigDigit).wrapping_shr((big_digit::BITS - nbs) as u32);
            rem_save.data.extend_from_slice(&x.data[..nws]);
            rem_save.data[nws - 1] &= mask;

            x_adj = x >> nshift;
            y_adj = y >> nshift;

            (&x_adj, &y_adj)
        } else {
            (x, y)
        };

        // Short cut for single odd-component divisor.
        if w.data.len() == 1 {
            let mut q = v.clone();
            let mut rem: BigUint = div_rem_digit(&mut q, w.data[0]).into();

            // Restore for the even case
            if nshift > 0 {
                rem <<= nshift;
                if rem.data.len() < rem_save.data.len() {
                    rem.data.resize(rem_save.data.len(), 0);
                }
                __add2(&mut rem.data, &rem_save.data);
            }

            return match (with_div, with_rem) {
                (true, true) => (Some(q), Some(rem.normalized())),
                (true, false) => (Some(q), None),
                (false, true) => (None, Some(rem.normalized())),
                (false, false) => (None, None),
            };
        }

        // The actual core of this function.

        // We require a modular inverse of w for modmul using montgomery.
        // let winv = mod_invn(w, scratch);

        /// Track if we need to regen cache.
        let regen_cache = if &cache.w == w { false } else { true };

        if regen_cache {
            cache.w = w.clone();
            cache.winv = mod_invn(w, scratch);
        }

        let winv = &cache.winv;

        // println!(
        //     "yinv = {}",
        //     BigUint {
        //         data: winv.data.clone()
        //     }
        // );

        // Process `v` in `w` len sized chunks. If the last chunk does not fit properly it gets special handling at the end.

        let v_len = x.data.len();
        let w_len = w.data.len();
        // lo: [..w_len], hi: [w_len..]
        let mut lohi = vec![0; 2 * w_len];
        cy.data.resize(w_len, 0);

        let v_chunks = v.data.chunks_exact(w_len);
        let rem = v_chunks.remainder();

        tmp.resize(w_len, 0);

        for vi in v_chunks {
            // for l in (0..(v_len - w_len + 1)).step_by(w_len) {
            // println!(
            //     "i = {}, v = {}",
            //     l,
            //     BigUint {
            //         data: v.data[i..i + w_len].into()
            //     }
            //     .normalized()
            // );
            tmp.copy_from_slice(vi);
            let bw = __sub2(tmp, &lohi[w_len..]);

            // Montgomery mul step: cy = UMULH(w, MULL(tmp, yinv))
            lohi[..w_len].copy_from_slice(&tmp);
            mul_lo(
                &mut lohi[..w_len],
                &winv.data[..std::cmp::min(winv.data.len(), w_len)],
                w_len,
                scratch,
            );

            // println!(
            //     "MULL = {}",
            //     BigUint {
            //         data: (&lohi[..w_len]).to_vec().into()
            //     }
            //     .normalized()
            // );

            assert!(__add_scalar(tmp, bw as BigDigit) == 0);

            // lo:hi = MUL_LOHI(w, tmp)
            let tmp2 = __mul3(&lohi[..w_len], &w.data, scratch);
            let cl = std::cmp::min(tmp.len(), lohi.len());
            lohi[..cl].copy_from_slice(&tmp2[..cl]);
            // println!(
            //     "  lo = {}",
            //     BigUint {
            //         data: (&lohi[..w_len]).to_vec().into()
            //     }
            //     .normalized()
            // );
            // println!(
            //     "  hi = {}",
            //     BigUint {
            //         data: (&lohi[w_len..]).to_vec().into()
            //     }
            //     .normalized()
            // );
        }

        // Special handling for the last term.
        // println!("i {}, j {}, {} {}", i, j, v_len, w_len);

        let j = rem.len();
        if j > 0 {
            // Zero pad the last element from v into cy.
            // println!("i+ = {}, v = {}", i, cy.clone().normalized());

            cy.data[..j].copy_from_slice(rem);

            for i in j..w_len {
                cy.data[i] = 0;
            }

            let bw = __sub2(&mut cy.data[..w_len], &lohi[w_len..]);
            if bw {
                __add2(&mut cy.data, &w.data);
            }
        } else {
            cy.data[..w_len].copy_from_slice(&lohi[..w_len]);
        }

        //println!("MR = {}", cy.clone().normalized());

        // Transform back out of Montgomery space.
        //   calculate R**w_len

        //  multiply the residue in `cy` with `basepow`
        if regen_cache {
            cache.basepow = radix_power_n(w_len, &w, &winv, scratch);
        }

        let mut cy = mont_mul_n(&cy, &cache.basepow, &w, &winv, scratch);

        // println!("remainder = {} ({:?})", &cy, &cy);

        if !with_div {
            return (None, Some(cy.normalized()));
        }

        // Calculate quotient, now that we have the remainder.
        let mut q = smallvec![0; 2 * w_len];
        let cl = std::cmp::min(cy.data.len(), w_len);
        lohi[w_len..w_len + cl].copy_from_slice(&cy.data[..cl]);
        if cl < w_len {
            lohi[w_len + cl..].copy_from_slice(&vec![0; w_len - cl]);
        }

        let v_chunks = v.data.chunks_exact(w_len);
        let rem = v_chunks.remainder();

        for (i, vi) in v_chunks.enumerate() {
            // for l in (0..v_len - w_len + 1).step_by(w_len) {
            // println!(
            //     "i = {}, v = {}",
            //     i,
            //     BigUint {
            //         data: v.data[i..i + w_len].into()
            //     }
            //     .normalized()
            // );
            tmp.copy_from_slice(vi);
            let bw = __sub2(tmp, &lohi[w_len..]);

            // Montgomery mul step
            mul_lo(tmp, &winv.data, w_len, scratch);
            // println!(
            //     "tmp * yinv = {}",
            //     BigUint {
            //         data: tmp.clone().into()
            //     }
            //     .normalized()
            // );

            for li in lohi.iter_mut().take(2 * w_len) {
                *li = 0;
            }
            mac3(&mut lohi[..2 * w_len], &tmp, &w.data);
            lohi[w_len] += bw as BigDigit;
            // println!("{:?}", &lohi);
            debug_assert!(lohi[w_len] >= bw as BigDigit, "carry issue");
            // println!(
            //     "  lo = {}",
            //     BigUint {
            //         data: (&lohi[..w_len]).to_vec().into()
            //     }
            //     .normalized()
            // );
            // println!(
            //     "  hi = {}",
            //     BigUint {
            //         data: (&lohi[w_len..]).to_vec().into()
            //     }
            //     .normalized()
            // );
            // this is the y[i] = tmp step in the scalar routine
            // println!("{:?}, {:?}, {}, {}", q, &tmp, i, w_len);
            if i + w_len >= q.len() {
                q.resize(i + w_len, 0);
            }
            q[i..i + w_len].copy_from_slice(&tmp[..w_len]);

            // println!("q: {}", BigUint { data: q.clone() }.normalized());
        }

        // handle last step, for non dividing cases
        // let j = v_len - i;

        let j = rem.len();
        // println!("i = {}, j = {}", i, j);
        if j > 0 {
            // clear q[i..j]
            let i = v_len / w_len;
            for qi in q.iter_mut().skip(i).take(j) {
                *qi = 0;
            }
        }
        // println!("q = {}", BigUint { data: q.clone() }.normalized());
        // Adjust remainder for even modulus.
        if nshift > 0 {
            cy <<= nshift;
            __add2(&mut cy.data, &rem_save.data);
        }

        match (with_div, with_rem) {
            (true, true) => (
                Some(BigUint { data: q }.normalized()),
                Some(cy.normalized()),
            ),
            (true, false) => (Some(BigUint { data: q }.normalized()), None),
            (false, true) => (None, Some(cy.normalized())),
            (false, false) => (None, None),
        }
    })
}

/// Shifts up to `63` bits to the left. Returning the out shifted bits.
fn shl_n(x: &mut [BigDigit], nbits: u32) -> BigDigit {
    let len = x.len();
    assert!(len > 0);
    assert!(nbits <= big_digit::BITS as u32);

    let mbits = big_digit::BITS as u32 - nbits;
    let ret = x[len - 1].wrapping_shr(mbits);

    for i in (1..len).rev() {
        x[i] = x[i].wrapping_shl(nbits) + x[i - 1].wrapping_shr(mbits);
    }

    x[0] = x[0].wrapping_shl(nbits);

    ret
}

/// Computes `R**n (mod q)`.
/// - `pow = R**2 (mod q)`
/// - `q * qinv = 1 (mod 2**BITS)`, `q` is odd.
fn radix_power_n(n: usize, q: &BigUint, qinv: &BigUint, scratch: &mut [BigDigit]) -> BigUint {
    let len = q.data.len();

    let mut r = BigUint {
        data: smallvec![0; 2 * len],
    };

    r.data[2 * len - 1] = big_digit::TWO_POW_BITS1;

    // println!("r/2 {}", &r);
    let (_, mut r2) = div_rem_knuth(&r, q);
    // println!("R**2/2 mod q = {}", &r2);
    r2.data.resize(len, 0);
    // println!("tmp {:?}", &r2.data);
    let overflow = shl_n(&mut r2.data, 1);
    // println!("tmp {:?}", &r2.data);

    r2.normalize();

    // println!("overflow {}", overflow);
    // println!("cmp {}", q < &r2);
    if overflow > 0 || q < &r2 {
        __sub2(&mut r2.data, &q.data);
        r2.normalize();
    }

    // println!("R**2 mod q = {}", &r2);
    if n == 2 {
        return r2;
    }

    // R**3 (mod q)

    if n == 3 {
        return mont_sqr_n(&r2, q, qinv, scratch);
    }

    let mut bm = 0u32;
    let mut j = 0;
    let mut n = n;
    while n > 5 {
        bm.set_bit(j, is_even(n));
        n = (n / 2) + 1;
        j += 1;
    }

    let r3 = mont_sqr_n(&r2, q, qinv, scratch);
    // println!("R**3 mod q = {}", &r3);
    let mut pow = if n == 4 {
        mont_mul_n(&r2, &r3, q, qinv, scratch)
    } else if n == 5 {
        mont_sqr_n(&r3, q, qinv, scratch)
    } else {
        unreachable!("bad power of p")
    };

    for i in (0..j).rev() {
        if bm.get_bit(i) {
            // Reduce R-power by 1
            let tmp = mmul_one_n(&pow, q, qinv, scratch);

            // and multiply
            pow = mont_mul_n(&tmp, &pow, q, qinv, scratch);
        } else {
            pow = mont_sqr_n(&pow, q, qinv, scratch);
        }
    }

    pow
}

/// Calculates the modular inverse of `x` , such that `x * xinv = 1 (mod 2**BITS)`.
/// `x` must be `odd`.
/// Uses Netwon iterations to do so.
fn mod_invn(x: &BigUint, scratch: &mut [BigDigit]) -> BigUint {
    debug_assert!(x.is_odd());

    let xbits = x.data.len() << 6; // TODO: adjust for 32bit
    let log2_numbits = ((1.0 * xbits as f64).ln() / (2.0f64.ln())).ceil() as usize;

    let mut xinv = BigUint {
        data: SmallVec::with_capacity(x.data.len()),
    };

    // inverse for one limb
    xinv.data.push(mod_inv1(x.data[0]));
    xinv.data.resize(x.data.len(), 0);

    // calculate the other limbs, we double the good limbs each iteration.
    // println!("x: {:?}", &x);
    // println!("xinv: {:?}", &xinv);

    let len = x.data.len();
    let mut tmp = vec![0; x.data.len()];

    for j in 6..log2_numbits {
        tmp.copy_from_slice(&x.data);
        // println!("j = {} / {}", j, log2_numbits);
        // println!("yinv = {}", xinv.clone().normalized());
        // println!("tmp = {}", BigUint { data: tmp.clone() });
        // println!("w = {}", &x);
        mul_lo(&mut tmp, &xinv.data, len, scratch);
        //println!("tmp: {:?}", &tmp);
        nega(&mut tmp);
        //println!("tmp: {:?}", &tmp);
        let _bw = __add_scalar(&mut tmp, 2);
        // debug_assert!(bw == 0, "unexpected carry");

        mul_lo(&mut xinv.data, &tmp, len, scratch);
    }

    xinv.normalize();
    //println!("yinv = {}", &xinv);
    xinv
}

/// Arithmetic negation, based on `-x = !x + 1`.
#[inline]
fn nega(x: &mut [BigDigit]) {
    negl(x);
    __add_scalar(x, 1);
}

/// Logical negation.
#[inline]
fn negl(x: &mut [BigDigit]) {
    for xi in x.iter_mut() {
        *xi = !*xi;
    }
}

fn div_rem_knuth(u: &BigUint, d: &BigUint) -> (BigUint, BigUint) {
    if d.is_zero() {
        panic!()
    }
    if u.is_zero() {
        return (Zero::zero(), Zero::zero());
    }
    if d.data.len() == 1 {
        if d.data[0] == 1 {
            return (u.clone(), Zero::zero());
        }

        let mut div = u.clone();
        let rem = div_rem_digit(&mut div, d.data[0]);
        return (div, rem.into());
    }

    // Required or the q_len calculation below can underflow:
    match u.cmp(d) {
        Ordering::Less => return (Zero::zero(), u.clone()),
        Ordering::Equal => return (One::one(), Zero::zero()),
        Ordering::Greater => {} // Do nothing
    }

    // This algorithm is from Knuth, TAOCP vol 2 section 4.3, algorithm D:
    //
    // First, normalize the arguments so the highest bit in the highest digit of the divisor is
    // set: the main loop uses the highest digit of the divisor for generating guesses, so we
    // want it to be the largest number we can efficiently divide by.
    //
    let shift = d.data.last().unwrap().leading_zeros() as usize;
    let mut a = u << shift;
    let b = d << shift;

    // The algorithm works by incrementally calculating "guesses", q0, for part of the
    // remainder. Once we have any number q0 such that q0 * b <= a, we can set
    //
    //     q += q0
    //     a -= q0 * b
    //
    // and then iterate until a < b. Then, (q, a) will be our desired quotient and remainder.
    //
    // q0, our guess, is calculated by dividing the last few digits of a by the last digit of b
    // - this should give us a guess that is "close" to the actual quotient, but is possibly
    // greater than the actual quotient. If q0 * b > a, we simply use iterated subtraction
    // until we have a guess such that q0 * b <= a.
    //

    let bn = *b.data.last().unwrap();
    let q_len = a.data.len() - b.data.len() + 1;
    let mut q = BigUint {
        data: smallvec![0; q_len],
    };

    // We reuse the same temporary to avoid hitting the allocator in our inner loop - this is
    // sized to hold a0 (in the common case; if a particular digit of the quotient is zero a0
    // can be bigger).
    //
    let mut tmp = BigUint {
        data: SmallVec::with_capacity(2),
    };

    for j in (0..q_len).rev() {
        /*
         * When calculating our next guess q0, we don't need to consider the digits below j
         * + b.data.len() - 1: we're guessing digit j of the quotient (i.e. q0 << j) from
         * digit bn of the divisor (i.e. bn << (b.data.len() - 1) - so the product of those
         * two numbers will be zero in all digits up to (j + b.data.len() - 1).
         */
        let offset = j + b.data.len() - 1;
        if offset >= a.data.len() {
            continue;
        }

        /* just avoiding a heap allocation: */
        let mut q0 = tmp;
        let len = a.data.len() - offset;
        q0.data.resize(len, 0);
        q0.data.copy_from_slice(&a.data[offset..]);

        /*
         * q0 << j * big_digit::BITS is our actual quotient estimate - we do the shifts
         * implicitly at the end, when adding and subtracting to a and q. Not only do we
         * save the cost of the shifts, the rest of the arithmetic gets to work with
         * smaller numbers.
         */

        div_rem_digit(&mut q0, bn);
        let mut prod = &b * &q0;

        while cmp_slice(&prod.data[..], &a.data[j..]) == Ordering::Greater {
            q0 -= &*ONE;
            prod -= &b;
        }

        add2(&mut q.data[j..], &q0.data[..]);
        sub2(&mut a.data[j..], &prod.data[..]);
        a.normalize();

        tmp = q0;
    }

    debug_assert!(a < b);

    (q.normalized(), a >> shift)
}

/// Slow division one bit at a time.
fn div_rem_binary(x: &BigUint, y: &BigUint) -> (BigUint, BigUint) {
    // println!("div_rem_binary {} / {}", x, y);
    assert!(!y.is_zero(), "dividing by 0");

    let x_len = x.data.len();
    let y_len = y.data.len();
    let max_len = std::cmp::max(x_len, y_len);

    // Done if `x < y`.
    if x_len < y_len || (x_len == y_len && x < y) {
        return (0u32.into(), x.clone());
    }

    let lz_x = x.leading_zeros() as usize + (max_len - x_len) * big_digit::BITS;
    let lz_y = y.leading_zeros() as usize + (max_len - y_len) * big_digit::BITS;
    let nshift = lz_y - lz_x;

    // Left justify the modulus
    let mut y_loc = y << nshift;
    let mut x_loc = x.clone();

    let mut q = BigUint {
        data: smallvec![0; max_len],
    };

    for i in (0..nshift + 1).rev() {
        if &x_loc > &y_loc {
            __sub2(&mut x_loc.data, &y_loc.data);
            x_loc.normalize();
            q.set_bit(i, true);
        }

        if i > 0 {
            y_loc >>= 1;
        }
    }

    // Remainder is in x_loc
    debug_assert!(&x_loc < y, "remainder should be less than the modulus");

    return (q.normalized(), x_loc);
}

/// Extract the leading `BITS` bits from `x`.
fn get_leading_bits(x: &BigUint) -> (BigDigit, usize) {
    let len = x.data.len();
    let nshift = x.leading_zeros() as usize;
    let nwshift = nshift / big_digit::BITS;
    let rembits = nshift & (big_digit::BITS - 1);

    if nwshift >= len {
        return (0, 0);
    }

    let i = len - 1 - nwshift;

    let bits = if rembits > 0 {
        let mut bits = x.data[i] << rembits;
        if i > 0 {
            bits += x.data[i - 1] >> (big_digit::BITS - rembits);
        }
        bits
    } else {
        x.data[i]
    };

    (bits, (len << 6) - nshift)
}

/// Returns the scaled remainder, `s = x * R**-n (mod q)`
/// `q * qinv = 1 (mod 2**BITS)`, `q` is odd.
fn remainder_a(x: &BigUint, q: BigDigit, qinv: BigDigit) -> BigDigit {
    debug_assert_eq!(q & 1, 1);
    debug_assert_eq!(q.wrapping_mul(qinv), 1);

    // special handling for q = 1
    if q == 1 {
        return 0;
    }

    let mut cy = 0;
    for xi in x.data.iter() {
        let (mut tmp, bw) = xi.overflowing_sub(cy);
        tmp = tmp.wrapping_mul(qinv) + bw as BigDigit;
        cy = umulh(tmp, q);
    }

    if cy > 0 {
        cy = q - cy;
    }

    // Final multiply to radix scale the remainder
    let n = x.data.len();
    let r_scaled = radix_power(n + 1, q, qinv);
    mont_mul(cy, r_scaled, q, qinv)
}

/// Returns the scaled remainder, `s = x * R**-n (mod q)`
/// `q * qinv = 1 (mod 2**BITS)`, `q` is odd.
fn remainder_a_u2(x: &BigUint, q: BigDigit, qinv: BigDigit) -> BigDigit {
    debug_assert_eq!(q & 1, 1);
    debug_assert_eq!(q.wrapping_mul(qinv), 1);

    let n = x.data.len();
    if n < 2 {
        return remainder_a(x, q, qinv);
    }
    // special handling for q = 1
    if q == 1 {
        return 0;
    }

    // Pad to even length and then divide by 2.
    let n2 = (n + (n & 1)) >> 1;

    let mut cy0 = 0;
    let mut cy1 = 0;

    for i in 0..n2 {
        let (mut tmp0, bw0) = x.data[i].overflowing_sub(cy0);
        let xin2 = if i + n2 < n {
            x.data[i + n2]
        } else {
            // Handle the case of odd limbs
            0
        };
        let (mut tmp1, bw1) = xin2.overflowing_sub(cy1);

        // Add q to loop output if there was a borrow
        tmp0 = tmp0.wrapping_mul(qinv) + bw0 as BigDigit;
        tmp1 = tmp1.wrapping_mul(qinv) + bw1 as BigDigit;

        cy0 = umulh(tmp0, q);
        cy1 = umulh(tmp1, q);
    }

    if cy0 > 0 {
        cy0 = q - cy0;
    }
    if cy1 > 0 {
        cy1 = q - cy1;
    }

    // P * R = R**n2+1 (mod q)
    let scale = radix_power(n2 + 1, q, qinv);

    cy1 = mont_mul(cy1, scale, q, qinv);

    // Sum the scaled partial remainders
    cy0 = cy0.wrapping_add(cy1);

    // Check for overflow on addition
    if cy0 < cy1 || cy0 >= q {
        cy0 = cy0.wrapping_sub(q);
    }

    mont_mul(cy0, scale, q, qinv)
}

/// Calculate the montgomery product `x * y * R**-1 (mod q)`,
/// with `R = 2**b`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
#[cfg(feature = "u64_digit")]
#[inline]
fn mont_mul(x: BigDigit, y: BigDigit, q: BigDigit, qinv: BigDigit) -> BigDigit {
    // println!("mont_mul: {} * {} (mod {})", x, y, q);
    debug_assert_eq!(q & 1, 1);

    if x == 0 || y == 0 {
        return 0;
    }

    let (hi, mut lo) = umul_lohi(x, y);

    lo = qinv.wrapping_mul(lo);
    lo = umulh(q, lo);

    // hi.wrapping_sub(lo)
    //     .wrapping_add((-((hi < lo) as i64)) as BigDigit & q)
    if hi < lo {
        q - (lo - hi)
    } else {
        hi - lo
    }
}

#[cfg(not(feature = "u64_digit"))]
#[inline]
fn mont_mul(x: BigDigit, y: BigDigit, q: BigDigit, qinv: BigDigit) -> BigDigit {
    mont_mul_32(x, y, q, qinv)
}

/// Calculate the montgomery product `x * y * R**-1 (mod q)`,
/// with `R = 2**32`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mont_mul_32(x: u32, y: u32, q: u32, qinv: u32) -> u32 {
    debug_assert_eq!(q & 1, 1);
    if x == 0 || y == 0 {
        return 0;
    }

    let (hi, mut lo) = umul_lohi_32(x, y);

    lo = qinv.wrapping_mul(lo);
    lo = umulh_32(q, lo);

    // hi.wrapping_sub(lo)
    //     .wrapping_add((-((hi < lo) as i32)) as u32 & q)
    // if hi < lo {
    //     hi.wrapping_sub(lo).wrapping_add(q)
    // } else {
    //     hi - lo
    // }

    if hi < lo {
        q - (lo - hi)
    } else {
        hi - lo
    }
}

/// Calculate the montgomery product `x * y * R**-1 (mod q)`,
/// with `R = 2**48`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mont_mul_48(x: u64, y: u64, q: u64, qinv: u64) -> u64 {
    debug_assert_eq!(q & 1, 1);

    let (hi, mut lo) = umul_lohi(x << 16, y);
    lo >>= 16;

    lo = qinv.wrapping_mul(lo);
    lo &= 0x0000FFFFFFFFFFFF;

    lo = umulh(q << 16, lo);

    // check for overlow
    hi.wrapping_sub(lo)
        .wrapping_add((-((hi < lo) as i64)) as u64 & q)
}

/// Calculate the montgomery product `x * y * R**-1 (mod q)`,
/// with `R = 2**b`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mont_mul_n(
    x: &BigUint,
    y: &BigUint,
    q: &BigUint,
    qinv: &BigUint,
    scratch: &mut [BigDigit],
) -> BigUint {
    // println!(
    //     "mont_mul_n: {} * {} (mod {})",
    //     x.clone().normalized(),
    //     y.clone().normalized(),
    //     q.clone().normalized()
    // );
    debug_assert!(q.is_odd());

    let len = x.data.len();
    let mut tmp = __mul3(&x.data, &y.data, scratch);
    if tmp.len() - len < q.data.len() {
        // pad so we can work with this
        tmp.resize(len + q.data.len(), 0);
    }
    let (lo, hi) = tmp.split_at_mut(len);

    mul_lo(lo, &qinv.data, len, scratch);
    mul_hi(lo, &q.data, len, scratch);

    let bw = __sub2(hi, &*lo);
    if bw {
        // println!("borrow");
        __add2(hi, &q.data);
    }

    BigUint {
        data: (&*hi).into(),
    }
    .normalized()
}

/// Calculate the montgomery square `x**2 * R**-1 (mod q)`,
/// with `R = 2**b`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mont_sqr(x: BigDigit, q: BigDigit, qinv: BigDigit) -> BigDigit {
    mont_mul(x, x, q, qinv)
}

/// Calculate the montgomery square `x**2 * R**-1 (mod q)`,
/// with `R = 2**b`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mont_sqr_n(x: &BigUint, q: &BigUint, qinv: &BigUint, scratch: &mut [BigDigit]) -> BigUint {
    // println!(
    //     "mont_sqr_n: {}**2 (mod {})",
    //     x.clone().normalized(),
    //     q.clone().normalized()
    // );
    debug_assert!(q.is_odd());
    let len = x.data.len();

    let mut tmp = __mul3(&x.data, &x.data, scratch);
    if tmp.len() - len < q.data.len() {
        // pad so we can work with this
        tmp.resize(len + q.data.len(), 0);
    }
    let (lo, hi) = tmp.split_at_mut(len);

    mul_lo(lo, &qinv.data, len, scratch);
    mul_hi(lo, &q.data, len, scratch);

    let bw = __sub2(hi, &*lo);
    if bw {
        // println!("borrow");
        __add2(hi, &q.data);
    }

    BigUint {
        data: (&*hi).into(),
    }
    .normalized()
}

/// Calculate the montgomery multiply by unity `x * R**-1 (mod q)`,
/// with `R = 2**b`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mmul_one(x: BigDigit, q: BigDigit, qinv: BigDigit) -> BigDigit {
    debug_assert_eq!(q & 1, 1);

    let mut lo = qinv.wrapping_mul(x);
    lo = umulh(q, lo);

    if lo != 0 {
        q - lo
    } else {
        lo
    }
}

/// Calculate the montgomery multiply by unity `x * R**-1 (mod q)`,
/// with `R = 2**b`.
/// `q * qinv = 1 (mod 2**b)`, `q` is odd.
fn mmul_one_n(x: &BigUint, q: &BigUint, qinv: &BigUint, scratch: &mut [BigDigit]) -> BigUint {
    debug_assert!(q.is_odd());

    let len = x.data.len();
    let mut z = x.data.clone();

    mul_lo(&mut z, &qinv.data, len, scratch);
    mul_hi(&mut z, &q.data, len, scratch);

    // hi - lo borrow, if hi == 0, then lo != 0.
    if !is_zero(&z) {
        sub2rev(&q.data, &mut z);
    }

    BigUint { data: z }.normalized()
}

#[inline]
fn calc_bitmap(n: usize) -> (usize, usize, u32) {
    let mut j = 0;
    let mut p = n;
    let mut bm = 0u32;

    while p > 5 {
        bm.set_bit(j, is_even(p));
        p = (p / 2) + 1;
        j += 1;
    }

    (j, p, bm)
}

/// Calculates `2**64 (mod q)`.
#[inline]
fn radix_power2_32(q: u32, qinv: u32) -> u32 {
    // println!("q = {}", q);

    // 2**63 (mod q)
    let mut res = (0x8000000000000000u64 % q as u64) as u32;
    // println!("2**63 {}", res);
    // 2**64 (mod q)
    let s = res.wrapping_sub(q);
    // println!("{} + {}", res, s);
    res = res.wrapping_add(s);
    // println!("= {}", res);
    let b = (res as i32) < 0;
    // println!("{}, {}, {}", res as i32, b, -(b as i32));
    let l = -(b as i32) as u32 & q;
    // println!("{} + {}", res, l);
    res = res.wrapping_add(l);
    // println!("= {}", res);

    res
}

/// Calculates `2**(BITS * 2) (mod q)`.
#[cfg(not(feature = "u64_digit"))]
#[inline]
fn radix_power2(q: BigDigit, qinv: BigDigit) -> BigDigit {
    radix_power2_32(q, qinv)
}

/// Calculates `2**(BITS * 2) (mod q)`.
#[cfg(feature = "u64_digit")]
fn radix_power2(q: BigDigit, qinv: BigDigit) -> BigDigit {
    if q.wrapping_shr(48) > 0 {
        let r = big_digit::TWO_POW_BITS as u128;
        let r_m = specialized_div_rem::u128_div_rem(r, q as u128).1;
        let r2 = r_m.wrapping_mul(r_m);

        return specialized_div_rem::u128_div_rem(r2, q as u128).1 as BigDigit;

        // TODO: Fix below for floating point fail

        // println!("64bits");
        // q in [2**48, 2**64]

        // this branch uses fast floating-point division.
        let fquo = TWO96FLOAT / (q as f64);

        // a := 2**96 , k := floor(a / q)
        //
        //        a = floor(a / q) * q + a % q
        // => a % q = a - floor(a / q) * q
        // => a % q = a - k * q
        // => a % q = 2**96 - floor(2**96 / q) * q
        //
        // q < 2**64 => q fits in 64 bits, so the result fits in 64 bits,
        // so we can do our ops using 64 bit math

        // floor(2**96 / q)
        let rem64 = fquo as u64;

        // Bottom 64 bits of 2**96  - q * floor(2**96 / q)
        let rhs = rem64.wrapping_mul(q);
        let res = (-(rhs as i64)) as u64;
        assert!(res <= q, "floating points failed us: {}", q);

        // Convert R**(3/2) to R**2.
        mont_sqr(res, q, qinv)
    } else if q.wrapping_shr(32) > 0 {
        // println!("48bits");
        // q in [2**32, 2**48)

        // 2**60 (mod q)
        let mut res = 0x1000000000000000u64 % q;
        // 2**72 (mod q)
        res = mont_mul_48(res, res, q, qinv);
        // 2**96 (mod q)
        res = mont_mul_48(res, res, q, qinv);
        // Convert R**(3/2) to R**2.
        mont_sqr(res, q, qinv)
    } else {
        // println!("32 bits");
        // q in [0, 2**32)
        let res = radix_power2_32(q as u32, qinv as u32);

        // 2**(2*64 - 32) = 2**96 (mod q)
        let res = mont_mul_32(res, res, q as u32, qinv as u32) as BigDigit;
        // println!("2**96 {}", res);
        let res = mont_sqr(res, q, qinv);
        // println!("2**64 {}", res);
        res
    }
}

/// Computes `R**n (mod q)`.
/// - `q * qinv = 1 (mod 2**BITS)`, `q` is odd.
fn radix_power(n: usize, q: BigDigit, qinv: BigDigit) -> BigDigit {
    let r2 = radix_power2(q, qinv);

    if n == 2 {
        return r2;
    }

    // R**3 (mod q)
    let mut ptmp = mont_sqr(r2, q, qinv);

    if n == 3 {
        return ptmp;
    }

    let (j, p, bm) = calc_bitmap(n);

    let mut pow = if p == 4 {
        mont_mul(r2, ptmp, q, qinv)
    } else {
        mont_sqr(ptmp, q, qinv)
    };

    // Bitmap controlled powering loop, only for n > 5
    for i in (0..j).rev() {
        if bm.get_bit(i) {
            // Reduce R-power by 1
            ptmp = mmul_one(pow, q, qinv);

            // and multiply
            pow = mont_mul(ptmp, pow, q, qinv);
        } else {
            pow = mont_sqr(pow, q, qinv)
        }
    }

    pow
}

#[inline(always)]
fn is_even(n: usize) -> bool {
    (n & 1) as u8 == 0
}

#[inline(always)]
fn umul_lohi(x: BigDigit, y: BigDigit) -> (BigDigit, BigDigit) {
    let res = x as DoubleBigDigit * y as DoubleBigDigit;
    (big_digit::get_hi(res), big_digit::get_lo(res))
}

#[inline(always)]
fn umul_lohi_32(x: u32, y: u32) -> (u32, u32) {
    let res = x as u64 * y as u64;
    ((res >> 32) as u32, res as u32)
}

/// Returns the scaled remainder, `s = x * R**-n (mod q)`
/// `q * qinv = 1 (mod 2**BITS)`, `q` is odd.
fn remainder_b(x: &BigUint, q: BigDigit, qinv: BigDigit) -> BigDigit {
    debug_assert_eq!(q & 1, 1);
    debug_assert_eq!(q.wrapping_mul(qinv), 1);

    let n = x.data.len();
    if n == 0 {
        return 0;
    }
    if n == 1 {
        return x.data[0] % q;
    }

    let mut cy = 0;

    for xi in x.data.iter().take(n - 1) {
        let (mut tmp, overflow) = xi.overflowing_sub(cy);

        tmp = (tmp.wrapping_mul(qinv)) + overflow as BigDigit;
        cy = umulh(tmp, q);
    }

    // Final term, without multiplication
    let xi = x.data[n - 1];

    let tmp = xi.wrapping_sub(cy);
    let lo = if cy > xi { tmp.wrapping_add(q) } else { tmp };

    // Final multiply to radix scale the remainder
    let n = x.data.len();
    let r_scaled = radix_power(n, q, qinv);

    mont_mul(lo, r_scaled, q, qinv)
}

/// Calculates the full double width multiply.
#[inline(always)]
fn usqr_lohi(x: BigDigit) -> (BigDigit, BigDigit) {
    umul_lohi(x, x)
}

/// Upper half multiply. Calculates `floor(x * y / R)`, with `R = 2**BITS`.
#[inline(always)]
fn umulh(x: BigDigit, y: BigDigit) -> BigDigit {
    big_digit::get_hi(x as DoubleBigDigit * y as DoubleBigDigit)
}

/// Upper half multiply. Calculates `floor(x * y / R)`, with `R = 2**32`.
#[inline(always)]
fn umulh_32(x: u32, y: u32) -> u32 {
    ((x as u64 * y as u64) >> 32) as u32
}

/// Calculate the quotient and remainder of `x / q`.
/// The quotient will be in `x` and the remainder is returned.
pub fn div_rem_digit(x: &mut BigUint, q: BigDigit) -> BigDigit {
    if x.is_zero() {
        return 0;
    }
    if x.data.len() == 1 {
        let rem = x.data[0] % q;
        x.data[0] /= q;
        x.normalize();
        return rem;
    }

    if q & 1 == 1 {
        let qinv = mod_inv1(q);

        let r = remainder_a_u2(&*x, q, qinv);

        quotient_odd(x, r, q, qinv);
        r
    } else {
        let tz = q.trailing_zeros();
        let bsave = x.data[0] & ((2 as BigDigit).pow(tz) - 1);

        let q_dash = q >> tz as usize;

        let qinv_dash = mod_inv1(q_dash);
        *x >>= tz as usize;

        let r_dash = remainder_b(&*x, q_dash, qinv_dash);

        quotient_odd(x, r_dash, q_dash, qinv_dash);

        (r_dash << tz as usize) + bsave
    }
}

/// Calculate the remainder of `x / q`.
/// The quotient will be returned.
pub fn rem_digit(x: &BigUint, q: BigDigit) -> BigDigit {
    if x.is_zero() {
        return 0;
    }
    if x.data.len() == 1 {
        return x.data[0] % q;
    }

    if q & 1 == 1 {
        let qinv = mod_inv1(q);
        remainder_a_u2(&x, q, qinv)
    } else {
        let tz = q.trailing_zeros();
        let bsave = x.data[0] & ((2 as BigDigit).pow(tz) - 1);

        let q_dash = q >> tz as usize;

        let qinv_dash = mod_inv1(q_dash);
        let x_dash = x >> tz as usize;

        let r_dash = remainder_b(&x_dash, q_dash, qinv_dash);

        (r_dash << tz as usize) + bsave
    }
}

/// Calculate the quotient of `x / q`.
/// The quotient will be in `x`.
pub fn div_digit(x: &mut BigUint, q: BigDigit) {
    if x.is_zero() {
        return;
    }

    if x.data.len() == 1 {
        x.data[0] /= q;
        x.normalize();
        return;
    }

    if q & 1 == 1 {
        let qinv = mod_inv1(q);
        let r = remainder_a_u2(&*x, q, qinv);

        quotient_odd(x, r, q, qinv);
    } else {
        let tz = q.trailing_zeros() as usize;

        let q_dash = q >> tz;
        let qinv_dash = mod_inv1(q_dash);
        *x >>= tz;

        let r_dash = remainder_b(&*x, q_dash, qinv_dash);

        quotient_odd(x, r_dash, q_dash, qinv_dash);
    }
}

fn quotient_odd(x: &mut BigUint, r: BigDigit, q: BigDigit, qinv: BigDigit) {
    let mut bw = 0;
    let mut cy = r;

    for xi in x.data.iter_mut() {
        let mut tmp = xi.wrapping_sub(bw).wrapping_sub(cy);
        bw = (tmp > *xi) as BigDigit;
        tmp = tmp.wrapping_mul(qinv);
        cy = umulh(tmp, q);

        *xi = tmp
    }

    debug_assert!(cy == 0, "cy: {} != 0", cy);
    x.normalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biguint::IntoBigUint;
    use crate::traits::ModInverse;

    use num_traits::{FromPrimitive, Num, Pow, ToPrimitive};
    use std::str::FromStr;

    #[test]
    fn test_is_even() {
        assert!(is_even(10));
        assert!(!is_even(11));
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_mont_sqr() {
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..100 {
            let x: BigDigit = rng.gen();
            let mut q: BigDigit = rng.gen();
            q |= 1;

            let qinv = mod_inv1(q);

            assert_eq!(
                mont_sqr(x, q, qinv),
                ((x as u128 * x as u128) % q as u128) as BigDigit
            );
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_mont_mul() {
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..100 {
            let x: BigDigit = rng.gen();
            let y: BigDigit = rng.gen();
            let mut q: BigDigit = rng.gen();
            q |= 1;

            let qinv = mod_inv1(q);

            assert_eq!(
                mont_mul(x, y, q, qinv),
                ((x as u128 * y as u128) % q as u128) as BigDigit
            );
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_mont_mul_32() {
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        let r = (u32::max_value() as u64) + 1;

        for _ in 0..100 {
            let x: u32 = rng.gen();
            let y: u32 = rng.gen();
            let mut q: u32 = rng.gen();
            q |= 1;

            let qinv = mod_inv1(q as BigDigit) as u32;
            let expected = x.wrapping_mul(y) % q;

            let p = mont_mul_32(x, y, q, qinv);

            // calc x * y (mod q) = p * rinv (mod q)
            let rinv = r
                .into_biguint()
                .unwrap()
                .mod_inverse(&q.into_biguint().unwrap())
                .unwrap()
                .into_biguint()
                .unwrap()
                .to_u64()
                .unwrap();

            assert_eq!(
                ((p as u64 * rinv) % q as u64) as u32,
                expected,
                "{} * {} * 2**-32 (mod {}) = {}",
                x,
                y,
                q,
                p
            );
        }
    }

    #[test]
    fn test_mmul_one() {
        let x = 1234;
        let q = 13;
        let qinv = mod_inv1(q);

        assert_eq!(mmul_one(x, q, qinv), mont_mul(x, 1, q, qinv));
    }

    #[test]
    #[cfg(feature = "u64_digit")]
    fn test_remainder_a_example() {
        let x: BigUint = (BigUint::from_u64(2).unwrap().pow(977u32)) - 1u32;
        let q = 16357897499336320049;
        let qinv = mod_inv1(q);

        assert_eq!(remainder_a(&x, q, qinv), 8623243291871090711);
    }

    #[test]
    #[cfg(feature = "u64_digit")]
    fn test_remainder_a_u2_example() {
        let x: BigUint = (BigUint::from_u64(2).unwrap().pow(977u32)) - 1u32;
        let q = 16357897499336320049;
        let qinv = mod_inv1(q);

        assert_eq!(remainder_a_u2(&x, q, qinv), 8623243291871090711);
    }

    #[test]
    #[cfg(feature = "u64_digit")]
    fn test_remainder_b_example() {
        let x: BigUint = (BigUint::from_u64(2).unwrap().pow(977u32)) - 1u32;
        let q = 16357897499336320049;
        let qinv = mod_inv1(q);

        assert_eq!(remainder_b(&x, q, qinv), 8623243291871090711);
    }

    #[test]
    #[cfg(feature = "u64_digit")]
    fn test_radix_power_example() {
        let q = 16357897499336320049;
        let qinv = mod_inv1(q);

        // Algorithm A:
        let pow = radix_power(17, q, qinv);

        let r17 = 8502984233828494641;
        assert_eq!(&pow, &r17);
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_radix_power2_range() {
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..1000 {
            let mut q: BigDigit = rng.gen();
            // make q odd
            q |= 1;

            let qinv = mod_inv1(q as BigDigit);
            let r2 = radix_power2(q as BigDigit, qinv);
            let expected = (big_digit::TWO_POW_BITS as u128 % q as u128) as BigDigit;

            assert_eq!(r2, expected, "failed to calc 2**(2*BITS) % {}", q);
        }
    }

    #[test]
    fn test_quotient_odd() {
        let mut x: BigUint = (BigUint::from_u64(2).unwrap().pow(977u32)) - 1u32;
        let q = 16357897499336320049;
        let qinv = mod_inv1(q);

        let rem = remainder_a_u2(&x, q, qinv);
        quotient_odd(&mut x, rem, q, qinv);
        let expected = BigUint::from_str("78086917842225469457022075217415018633622146158582987787805457927845552003930951370242413093007381680736663345444780010948879462256334087427082857530164140957807257857039967815743361429510512762352923129675520587113443817607507240658518046987342885964515476672818868436366440").unwrap();

        assert_eq!(x, expected);
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_div_rem_digit() {
        use rand::{thread_rng, Rng};
        let mut rng = thread_rng();

        for _ in 1..1000 {
            let x_orig: BigDigit = rng.gen();
            let mut x: BigUint = x_orig.into();
            let q: BigDigit = rng.gen();

            let rem = div_rem_digit(&mut x, q);
            assert_eq!(x_orig % q, rem);
            assert_eq!(x_orig / q, x.to_u64().unwrap());
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_remainder_odd_range() {
        use rand::{thread_rng, Rng};
        let mut rng = thread_rng();

        for _ in 1..1000 {
            let x_orig: BigDigit = rng.gen();
            let mut x: BigUint = x_orig.into();
            let mut q: BigDigit = rng.gen();
            // make q odd
            q |= 1;
            let qinv = mod_inv1(q);

            let rem = remainder_a_u2(&x, q, qinv);
            assert_eq!(x_orig % q, rem);
        }
    }

    #[test]
    fn test_negl() {
        let mut x = vec![1, 2, 3];
        negl(&mut x);
        assert_eq!(x, vec![!1, !2, !3]);
    }

    #[test]
    fn test_nega() {
        let mut x = vec![5];
        nega(&mut x);
        assert_eq!(x, vec![!5 + 1]);
    }

    #[test]
    fn test_div_rem_binary() {
        let x = BigUint::from_u64(123).unwrap();
        let y = BigUint::from_u64(456).unwrap();

        let q: u32 = 123 / 456;
        let r: u32 = 123 % 456;

        assert_eq!(div_rem_binary(&x, &y), (q.into(), r.into()));

        let x = BigUint::from_str_radix("364131549958466711308970009901738230041", 10).unwrap();
        let y = BigUint::from_str_radix("19437941122649628431", 10).unwrap();

        let q = BigUint::from_str_radix("18733030811281269087", 10).unwrap();
        let r = BigUint::from_str_radix("618006351061617544", 10).unwrap();

        assert_eq!(div_rem_binary(&x, &y), (q.into(), r.into()));

        let x = BigUint::from_str_radix(
            "127448010132852514736270810920533549640629212529195061317911604628433231530694485239219883465657029494279756403108733909500313306050993696584122751370335550177025678063778658",
            10,
        )
        .unwrap();
        let y = BigUint::from_str_radix(
            "3843712304801279920630164197378272649748715177882209365334",
            10,
        )
        .unwrap();

        let q = BigUint::from_str_radix("33157531060182086635190557281747640466913923624535236166568672556136939987187181635382537060699711037601239029963558", 10).unwrap();
        let r = BigUint::from_str_radix(
            "3721863193561123777824711600049067336224261079365735280286",
            10,
        )
        .unwrap();

        assert_eq!(div_rem_binary(&x, &y), (q, r));
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_mont_mul_n_rand() {
        use crate::bigrand::RandBigInt;
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);
        let mut scratch = vec![0; 512];

        for _ in 0..100 {
            let x = rng.gen_biguint(1024);
            let y = rng.gen_biguint(1024);
            let mut q = rng.gen_biguint(1024);
            q |= BigUint::one();

            let qinv = mod_invn(&q, &mut scratch);

            let rinv = BigUint::from_u64(2)
                .unwrap()
                .pow(big_digit::BITS * x.data.len())
                .into_biguint()
                .unwrap()
                .mod_inverse(&q)
                .unwrap()
                .into_biguint()
                .unwrap();

            let (_, res) = div_rem_knuth(&(&x * &y * &rinv), &q);
            assert_eq!(mont_mul_n(&x, &y, &q, &qinv, &mut scratch), res,);
        }
    }

    #[test]
    fn test_mont_mul_n_example() {
        let x: BigUint = 10u32.into();
        let y: BigUint = 40u32.into();
        let q: BigUint = 13u32.into();
        let mut scratch = vec![0; 6];

        let qinv = mod_invn(&q, &mut scratch);

        let res = mont_mul_n(&x, &y, &q, &qinv, &mut scratch);
        let r = 2u128.pow(big_digit::BITS as u32);
        let rinv = r
            .into_biguint()
            .unwrap()
            .mod_inverse(&q)
            .unwrap()
            .into_biguint()
            .unwrap();
        let expected = (&x * &y * rinv) % q;

        assert_eq!(res, expected);
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_mont_sqr_n() {
        use crate::bigrand::RandBigInt;
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);
        let mut scratch = vec![0; 512];

        for i in 0..1000 {
            let x = rng.gen_biguint(1024);
            let mut q = rng.gen_biguint(1024);
            q |= BigUint::one();

            let qinv = mod_invn(&q, &mut scratch);

            let res = mont_sqr_n(&x, &q, &qinv, &mut scratch);
            let r = 2u128.pow(big_digit::BITS as u32);
            let rinv = r
                .into_biguint()
                .unwrap()
                .mod_inverse(&q)
                .unwrap()
                .into_biguint()
                .unwrap();
            let expected = (&x * &x * rinv) % q;

            assert_eq!(res, expected);
        }
    }

    #[test]
    fn test_mod_invn_cases() {
        let y = BigUint::from_str_radix("19437941122649628431", 10).unwrap();
        let mut scratch = vec![0; 4];

        assert_eq!(
            mod_invn(&y, &mut scratch),
            BigUint::from_str_radix("232515390938603038445704290069949444079", 10).unwrap()
        );

        let y = BigUint::from_str_radix(
            "1921856152400639960315082098689136324874357588941104682667",
            10,
        )
        .unwrap();

        assert_eq!(
            mod_invn(&y, &mut scratch),
            BigUint::from_str_radix(
                "3403229015187975702927478358156698358619536564089218675715",
                10
            )
            .unwrap()
        );
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_mod_invn_rand() {
        use crate::bigrand::RandBigInt;
        use rand::thread_rng;

        let mut rng = thread_rng();
        let mut scratch = vec![0; 1024];
        for i in 0..1000 {
            for b in &[64, 128, 256, 1024, 2048] {
                let mut x = rng.gen_biguint(*b);
                // make x odd
                x.set_bit(0, true);
                let xinv = mod_invn(&x, &mut scratch);
                let r = 2u32
                    .into_biguint()
                    .unwrap()
                    .pow((big_digit::BITS * x.data.len()) as u32);

                assert_eq!((&x * &xinv) % r, BigUint::one(), "{} * {}", &x, &xinv);
            }
        }
    }

    #[test]
    fn test_div_rem_mont() {
        let x = BigUint::from_u64(123).unwrap();
        let y = BigUint::from_u64(456).unwrap();

        let q: u32 = 123 / 456;
        let r: u32 = 123 % 456;

        assert_eq!(div_rem_mont(&x, &y), (q.into(), r.into()));

        let x = BigUint::from_str_radix("364131549958466711308970009901738230041", 10).unwrap();
        let y = BigUint::from_str_radix("19437941122649628431", 10).unwrap();

        let q = BigUint::from_str_radix("18733030811281269087", 10).unwrap();
        let r = BigUint::from_str_radix("618006351061617544", 10).unwrap();

        assert_eq!(div_rem_mont(&x, &y), (q.into(), r.into()));

        let x = BigUint::from_str_radix(
            "26959946667150639794667015086679348306636977796562933536078298087425",
            10,
        )
        .unwrap();
        let y = BigUint::from_str_radix("79228162514264337593543950335", 10).unwrap();

        let q = BigUint::from_str_radix("340282366920938463463374607431768211455", 10).unwrap();
        let r = BigUint::from_str_radix("0", 10).unwrap();

        assert_eq!(div_rem_mont(&x, &y), (q.into(), r.into()));

        let x = BigUint::from_str_radix(
            "127448010132852514736270810920533549640629212529195061317911604628433231530694485239219883465657029494279756403108733909500313306050993696584122751370335550177025678063778658",
            10,
        )
        .unwrap();
        let y = BigUint::from_str_radix(
            "3843712304801279920630164197378272649748715177882209365334",
            10,
        )
        .unwrap();

        let q = BigUint::from_str_radix("33157531060182086635190557281747640466913923624535236166568672556136939987187181635382537060699711037601239029963558", 10).unwrap();
        let r = BigUint::from_str_radix(
            "3721863193561123777824711600049067336224261079365735280286",
            10,
        )
        .unwrap();

        assert_eq!(div_rem_mont(&x, &y), (q, r));
    }

    #[test]
    fn test_radix_power_n() {
        let q: BigUint = 16357897499336320049u64.into();
        let mut scratch = vec![0; 8];
        let qinv = mod_invn(&q, &mut scratch);

        // Algorithm A:
        let pow = radix_power_n(17, &q, &qinv, &mut scratch);

        let r17: BigUint = 8502984233828494641u64.into();
        assert_eq!(&pow, &r17);
    }

    #[test]
    fn test_rem_mont_cases() {
        let cases: Vec<[&str; 2]> = vec![
["79144329350142627392718293582040160529426246424927819703840411282938052706136", "39097044966705719959721893405030881015972913594713944607380410701191666649246"],
["39097044966705719959721893405030881015972913594713944607380410701191666649246", "950239416731187473274506771978398497480419235499930489079589880554719407644"],
["950239416731187473274506771978398497480419235499930489079589880554719407644", "137228880727033555467115753916542619275724939216794555117225598448170935842"],
["137228880727033555467115753916542619275724939216794555117225598448170935842", "126866132368986140471812248479142781826069600199163158376236289865693792592"],
["126866132368986140471812248479142781826069600199163158376236289865693792592", "10362748358047414995303505437399837449655339017631396740989308582477143250"],
["10362748358047414995303505437399837449655339017631396740989308582477143250", "2513152072417160528170183230344732430205531987586397484364586875968073592"],
["2513152072417160528170183230344732430205531987586397484364586875968073592", "310140068378772882622772516020907728833211067285806803530961078604848882"],
["310140068378772882622772516020907728833211067285806803530961078604848882", "32031525386977467188003102177470599539843449299943056116898247129282536"],
["32031525386977467188003102177470599539843449299943056116898247129282536", "21856339895975677930744596423672332974620023586319298478876854441306058"],
["21856339895975677930744596423672332974620023586319298478876854441306058", "10175185491001789257258505753798266565223425713623757638021392687976478"],
["10175185491001789257258505753798266565223425713623757638021392687976478", "1505968913972099416227584916075799844173172159071783202834069065353102"],
["1505968913972099416227584916075799844173172159071783202834069065353102", "1139372007169192759892996257343467500184392759193058421016978295857866"],
["1139372007169192759892996257343467500184392759193058421016978295857866", "366596906802906656334588658732332343988779399878724781817090769495236"],
["366596906802906656334588658732332343988779399878724781817090769495236", "39581286760472790889230281146470468218054559556884075565705987372158"],
["39581286760472790889230281146470468218054559556884075565705987372158", "10365325958651538331516128414098130026288363866768101725736883145814"],
["10365325958651538331516128414098130026288363866768101725736883145814", "8485308884518175894681895904176078139189467956579770388495337934716"],
["8485308884518175894681895904176078139189467956579770388495337934716", "1880017074133362436834232509922051887098895910188331337241545211098"],
["1880017074133362436834232509922051887098895910188331337241545211098", "965240587984726147344965864487870590793884315826445039529157090324"],
["965240587984726147344965864487870590793884315826445039529157090324", "914776486148636289489266645434181296305011594361886297712388120774"],
["914776486148636289489266645434181296305011594361886297712388120774", "50464101836089857855699219053689294488872721464558741816768969550"],
["50464101836089857855699219053689294488872721464558741816768969550", "6422653099018848086680702467773995505302607999828945010546668874"],
["6422653099018848086680702467773995505302607999828945010546668874", "5505530142957921248934301779271325951754465465756126742942287432"],
["5505530142957921248934301779271325951754465465756126742942287432", "917122956060926837746400688502669553548142534072818267604381442"],
["917122956060926837746400688502669553548142534072818267604381442", "2792406592360222455897648255308630465610261319217137315998780"],
["2792406592360222455897648255308630465610261319217137315998780", "1213593766773872211972060761438760827976821369597227956781602"],
["1213593766773872211972060761438760827976821369597227956781602", "365219058812478031953526732431108809656618580022681402435576"],
["365219058812478031953526732431108809656618580022681402435576", "117936590336438116111480564145434399006965629529183749474874"],
["117936590336438116111480564145434399006965629529183749474874", "11409287803163683619085039994805612635721691435130154010954"],
["11409287803163683619085039994805612635721691435130154010954", "3843712304801279920630164197378272649748715177882209365334"],
["2491393032846003075150380609947560862705074326848279793552226384070639711231603806802520494545287192134777473211116", "3843712304801279920630164197378272649748715177882209365334"],
            ["48210658914464466251348004742999945385157318276486079313177336918887707197165786181936910847295858955200920058008388226425694187063838159074756810583793021935748915977003942", "3843712304801279920630164"],
            ["494660802946209068121005042039294380070262698202423679828126112185794450213063734340632802122486089979195342852032277968801823228010993608887708077777044096365124400224210850", "3843712304801279920630164197378272649748715177882209365334"],
            [
                "3843712304801279920630164197378272649748715177882209365334",
                "121849111240156142805452597329205313524454098516474085048",
            ],
            [
                "121849111240156142805452597329205313524454098516474085048",
                "66389856356439493661133680172907930490638123871512728846",
            ],
            [
                "66389856356439493661133680172907930490638123871512728846",
                "55459254883716649144318917156297383033815974644961356202",
            ],
            [
                "55459254883716649144318917156297383033815974644961356202",
                "10930601472722844516814763016610547456822149226551372644",
            ],
            [
                "10930601472722844516814763016610547456822149226551372644",
                "806247520102426560245102073244645749705228512204492982",
            ],
            [
                "806247520102426560245102073244645749705228512204492982",
                "449383711391299233628436064430152710654178567892963878",
            ],
            [
                "449383711391299233628436064430152710654178567892963878",
                "356863808711127326616666008814493039051049944311529104",
            ],
            [
                "356863808711127326616666008814493039051049944311529104",
                "92519902680171907011770055615659671603128623581434774",
            ],
            [
                "92519902680171907011770055615659671603128623581434774",
                "79304100670611605581355841967514024241664073567224782",
            ],
            [
                "79304100670611605581355841967514024241664073567224782",
                "13215802009560301430414213648145647361464550014209992",
            ],
            [
                "13215802009560301430414213648145647361464550014209992",
                "9288613249796998870560078640140072876773481964830",
            ],
            [
                "9288613249796998870560078640140072876773481964830",
                "7393968348969036477781821866463730692658660221732",
            ],
            [
                "7393968348969036477781821866463730692658660221732",
                "1894644900827962392778256773676342184114821743098",
            ],
            [
                "1894644900827962392778256773676342184114821743098",
                "1710033646485149299447051545434704140314194992438",
            ],
            [
                "1710033646485149299447051545434704140314194992438",
                "184611254342813093331205228241638043800626750660",
            ],
            [
                "184611254342813093331205228241638043800626750660",
                "48532357399831459466204491259961746108554236498",
            ],
            [
                "48532357399831459466204491259961746108554236498",
                "39014182143318714932591754461752805474964041166",
            ],
            [
                "39014182143318714932591754461752805474964041166",
                "9518175256512744533612736798208940633590195332",
            ],
            [
                "9518175256512744533612736798208940633590195332",
                "941481117267736798140807268917042940603259838",
            ],
            [
                "941481117267736798140807268917042940603259838",
                "103364083835376552204664109038511227557596952",
            ],
            [
                "103364083835376552204664109038511227557596952",
                "11204362749347828298830287570441892584887270",
            ],
            [
                "11204362749347828298830287570441892584887270",
                "2524819091246097515191520904534194293611522",
            ],
            [
                "2524819091246097515191520904534194293611522",
                "1105086384363438238064203952305115410441182",
            ],
            [
                "1105086384363438238064203952305115410441182",
                "314646322519221039063112999923963472729158",
            ],
            [
                "314646322519221039063112999923963472729158",
                "161147416805775120874864952533224992253708",
            ],
            [
                "161147416805775120874864952533224992253708",
                "153498905713445918188248047390738480475450",
            ],
            [
                "153498905713445918188248047390738480475450",
                "7648511092329202686616905142486511778258",
            ],
            [
                "7648511092329202686616905142486511778258",
                "528683866861864455909944541008244910290",
            ],
            [
                "528683866861864455909944541008244910290",
                "246936956263100303877681568371083034198",
            ],
            [
                "13658002240333739429480613357949475702556350853006782024046489552116400922332",
                "246936956263100303877681568371083034198",
            ],
            [
                "246936956263100303877681568371083034198",
                "212127001927436455723100164105004192304",
            ],
            [
                "212127001927436455723100164105004192304",
                "34809954335663848154581404266078841894",
            ],
            [
                "34809954335663848154581404266078841894",
                "3267275913453366795611738508531140940",
            ],
            [
                "3267275913453366795611738508531140940",
                "2137195201130180198464019180767432494",
            ],
            [
                "2137195201130180198464019180767432494",
                "1130080712323186597147719327763708446",
            ],
            [
                "1130080712323186597147719327763708446",
                "1007114488806993601316299853003724048",
            ],
            [
                "1007114488806993601316299853003724048",
                "122966223516192995831419474759984398",
            ],
            [
                "122966223516192995831419474759984398",
                "23384700677449634664944054923848864",
            ],
            [
                "23384700677449634664944054923848864",
                "6042720128944822506699200140740078",
            ],
            [
                "6042720128944822506699200140740078",
                "5256540290615167144846454501628630",
            ],
            [
                "5256540290615167144846454501628630",
                "786179838329655361852745639111448",
            ],
            [
                "786179838329655361852745639111448",
                "539461260637234973729980666959942",
            ],
            [
                "539461260637234973729980666959942",
                "246718577692420388122764972151506",
            ],
            [
                "246718577692420388122764972151506",
                "46024105252394197484450722656930",
            ],
            [
                "46024105252394197484450722656930",
                "16598051430449400700511358866856",
            ],
            [
                "16598051430449400700511358866856",
                "12828002391495396083428004923218",
            ],
            [
                "12828002391495396083428004923218",
                "3770049038954004617083353943638",
            ],
            [
                "3770049038954004617083353943638",
                "1517855274633382232177943092304",
            ],
            [
                "1517855274633382232177943092304",
                "734338489687240152727467759030",
            ],
            [
                "734338489687240152727467759030",
                "49178295258901926723007574244",
            ],
            [
                "49178295258901926723007574244",
                "45842356062613178605361719614",
            ],
            [
                "45842356062613178605361719614",
                "3335939196288748117645854630",
            ],
            [
                "3335939196288748117645854630",
                "2475146510859453075965609424",
            ],
            [
                "2475146510859453075965609424",
                "860792685429295041680245206",
            ],
            ["860792685429295041680245206", "753561140000862992605119012"],
            ["753561140000862992605119012", "107231545428432049075126194"],
            ["107231545428432049075126194", "2940322001838649079235654"],
            ["2940322001838649079235654", "1379953362240682222642650"],
            ["1379953362240682222642650", "180415277357284633950354"],
            ["180415277357284633950354", "117046420739689784990172"],
            ["117046420739689784990172", "63368856617594848960182"],
            ["63368856617594848960182", "53677564122094936029990"],
            ["53677564122094936029990", "9691292495499912930192"],
            ["9691292495499912930192", "5221101644595371379030"],
            ["5221101644595371379030", "4470190850904541551162"],
            ["4470190850904541551162", "750910793690829827868"],
            ["750910793690829827868", "715636882450392411822"],
            ["715636882450392411822", "35273911240437416046"],
        ];

        let results: Vec<[&str; 2]> = vec![
            ["1", "34809954335663848154581404266078841894"],
            ["6", "3267275913453366795611738508531140940"],
            ["10", "2137195201130180198464019180767432494"],
            ["1", "1130080712323186597147719327763708446"],
            ["1", "1007114488806993601316299853003724048"],
            ["1", "122966223516192995831419474759984398"],
            ["8", "23384700677449634664944054923848864"],
            ["5", "6042720128944822506699200140740078"],
            ["3", "5256540290615167144846454501628630"],
            ["1", "786179838329655361852745639111448"],
            ["6", "539461260637234973729980666959942"],
            ["1", "246718577692420388122764972151506"],
            ["2", "46024105252394197484450722656930"],
            ["5", "16598051430449400700511358866856"],
            ["2", "12828002391495396083428004923218"],
            ["1", "3770049038954004617083353943638"],
            ["3", "1517855274633382232177943092304"],
            ["2", "734338489687240152727467759030"],
            ["2", "49178295258901926723007574244"],
            ["14", "45842356062613178605361719614"],
            ["1", "3335939196288748117645854630"],
            ["13", "2475146510859453075965609424"],
            ["1", "860792685429295041680245206"],
            ["2", "753561140000862992605119012"],
            ["1", "107231545428432049075126194"],
            ["7", "2940322001838649079235654"],
            ["36", "1379953362240682222642650"],
            ["2", "180415277357284633950354"],
            ["7", "117046420739689784990172"],
            ["1", "63368856617594848960182"],
            ["1", "53677564122094936029990"],
            ["1", "9691292495499912930192"],
            ["5", "5221101644595371379030"],
            ["1", "4470190850904541551162"],
            ["1", "750910793690829827868"],
            ["5", "715636882450392411822"],
            ["1", "35273911240437416046"],
            ["20", "10158657641644090902"],
        ];
        for (case, res) in cases.iter().zip(&results) {
            let x = BigUint::from_str_radix(case[0], 10).unwrap();
            let y = BigUint::from_str_radix(case[1], 10).unwrap();

            let r1 = div_rem_mont(&x, &y);
            let r2 = div_rem_binary(&x, &y);
            let expected = (
                BigUint::from_str_radix(res[0], 10).unwrap(),
                BigUint::from_str_radix(res[1], 10).unwrap(),
            );
            assert_eq!(&r1, &r2);
            // assert_eq!(&r1, &expected);
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_umulh_lohi_32() {
        use crate::bigrand::RandBigInt;
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..100 {
            let x: u32 = rng.gen();
            let y: u32 = rng.gen();

            let (hi, lo) = umul_lohi_32(x, y);
            let lohi = ((hi as u64) << 32) | (lo as u64);
            assert_eq!(lohi, x as u64 * y as u64);
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_umulh_lohi() {
        use crate::bigrand::RandBigInt;
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..100 {
            let x: BigDigit = rng.gen();
            let y: BigDigit = rng.gen();

            let (hi, lo) = umul_lohi(x, y);
            let lohi = ((hi as DoubleBigDigit) << BITS) | (lo as DoubleBigDigit);
            assert_eq!(lohi, x as DoubleBigDigit * y as DoubleBigDigit);
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_umulh_32() {
        use crate::bigrand::RandBigInt;
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..100 {
            let x: u32 = rng.gen();
            let y: u32 = rng.gen();

            let expected = ((x as u64 * y as u64) >> 32) as u32;
            assert_eq!(umulh_32(x, y), expected);
        }
    }

    #[test]
    #[cfg(feature = "rand")]
    fn test_umulh() {
        use crate::bigrand::RandBigInt;
        use rand::{Rng, SeedableRng, XorShiftRng};
        let mut rng = XorShiftRng::from_seed([1u8; 16]);

        for _ in 0..100 {
            let x: BigDigit = rng.gen();
            let y: BigDigit = rng.gen();

            let expected = ((x as DoubleBigDigit * y as DoubleBigDigit) >> BITS) as BigDigit;
            assert_eq!(umulh(x, y), expected);
        }
    }
}
