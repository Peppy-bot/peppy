/// Arm identifier — 0 = left, 1 = right.
/// Newtype prevents passing gripper_id where arm_id is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmId(pub u8);

impl ArmId {
    pub fn as_u8(self) -> u8 {
        self.0
    }
}

impl From<u8> for ArmId {
    fn from(v: u8) -> Self {
        Self(v)
    }
}

/// Simulation or real-time step counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StepId(pub u64);

impl StepId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Ordered joint position values for a single arm or full robot.
#[derive(Debug, Clone, PartialEq)]
pub struct JointPositions(pub Vec<f64>);

impl JointPositions {
    pub fn as_slice(&self) -> &[f64] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<f64> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<f64>> for JointPositions {
    fn from(v: Vec<f64>) -> Self {
        Self(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_id_roundtrip() {
        let id = ArmId::from(1u8);
        assert_eq!(id.as_u8(), 1);
    }

    #[test]
    fn step_id_increment() {
        let s = StepId(41);
        assert_eq!(s.next().as_u64(), 42);
    }

    #[test]
    fn joint_positions_len() {
        let jp = JointPositions::from(vec![0.1, 0.2, 0.3]);
        assert_eq!(jp.len(), 3);
        assert!(!jp.is_empty());
    }
}
