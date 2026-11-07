use super::Transcript;

#[derive(Clone)]
pub struct Blake3Transcript {
    pub state: blake3::Hasher,
    /// Pending XOF output stream. Created on the first squeeze after an
    /// absorb, advanced by every consecutive squeeze, invalidated by the next
    /// absorb. Without this, blake3's `finalize_xof` restarts at stream
    /// position 0 on every call and *consecutive* squeezes (no absorb between)
    /// return identical bytes — degenerating RLC coefficients and
    /// multi-coordinate challenge points to constants.
    xof: Option<blake3::OutputReader>,
}

impl Blake3Transcript {
    pub fn new(dst: &[u8]) -> Self {
        let mut state = blake3::Hasher::new();
        state.update(dst);
        Self { state, xof: None }
    }
}

impl Transcript for Blake3Transcript {
    fn append_bytes(&mut self, bytes: &[u8]) {
        // absorbing invalidates the pending output stream
        self.xof = None;
        self.state.update(bytes);
    }

    fn challenge_bytes(&mut self, out: &mut [u8]) {
        if self.xof.is_none() {
            self.xof = Some(self.state.finalize_xof());
        }
        self.xof
            .as_mut()
            .expect("xof stream just created")
            .fill(out);
    }
}

#[cfg(test)]
mod test {
    use ark_ff::UniformRand;
    use ark_std::rand::thread_rng;
    use itertools::Itertools;

    use crate::arkyper::transcript::{Transcript, blake3::Blake3Transcript};
    type F = ark_bn254::Fq;
    type A = ark_bn254::G1Affine;

    #[test]
    fn test_consecutive_challenges_distinct() {
        // S1 回归：同一吸收后的连续采样必须互异（XOF 流推进），
        // 且吸收后首个采样与旧语义一致（从流位置 0 开始）
        use crate::arkyper::transcript::VectorTranscript;
        type Fr = ark_bn254::Fr;
        let mut t = Blake3Transcript::new(b"test");
        t.append_bytes(b"absorb once");
        let chals: Vec<Fr> = t.read_challenges(8);
        for i in 0..chals.len() {
            for j in (i + 1)..chals.len() {
                assert_ne!(chals[i], chals[j], "consecutive challenges must differ");
            }
        }
        // 单采样语义不变：重新初始化 + 相同吸收 → 相同首采样
        let mut t2 = Blake3Transcript::new(b"test");
        t2.append_bytes(b"absorb once");
        let first = t2.challenge_scalar::<Fr>();
        assert_eq!(first, chals[0], "first challenge after absorb must be stable");
    }

    #[test]
    fn test_transcript() {
        let init_transcript = || Blake3Transcript::new(b"test");
        let mut t = init_transcript();
        // generate random vector of field elements
        let scalars = (0..100).map(|_| F::rand(&mut thread_rng())).collect_vec();
        t.append_scalars(&scalars);
        // squeeze challenge
        let challenge = t.challenge_scalar::<F>();

        // re-initialize transcript and check consistency
        let mut t = init_transcript();
        scalars.iter().for_each(|s| t.append_scalars::<F>(&[s]));
        // get challenge
        let chal_2 = t.challenge_scalar();

        assert_eq!(challenge, chal_2);

        // same test but for points
        let points = (0..100).map(|_| A::rand(&mut thread_rng())).collect_vec();
        let mut t = init_transcript();
        t.append_points(&points);
        let challenge = t.challenge_scalar::<F>();

        // re-initialize transcript and check consistency
        let mut t = init_transcript();
        points.iter().for_each(|a| t.append_points::<A>(&[a]));
        // get challenge
        let chal_2 = t.challenge_scalar();

        assert_eq!(challenge, chal_2);
    }
}
