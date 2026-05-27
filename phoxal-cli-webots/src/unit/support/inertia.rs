use nalgebra::{Isometry3, Matrix3, Vector3};

use crate::unit::support::urdf::pose_to_isometry;

#[derive(Debug, Clone)]
pub struct ResolvedInertial {
    pub origin: Isometry3<f64>,
    pub mass: f64,
    pub inertia: InertiaMatrix,
}

#[derive(Debug, Clone)]
pub struct InertiaMatrix {
    pub ixx: f64,
    pub ixy: f64,
    pub ixz: f64,
    pub iyy: f64,
    pub iyz: f64,
    pub izz: f64,
}

impl InertiaMatrix {
    fn to_matrix(&self) -> Matrix3<f64> {
        Matrix3::new(
            self.ixx, self.ixy, self.ixz, self.ixy, self.iyy, self.iyz, self.ixz, self.iyz,
            self.izz,
        )
    }

    fn from_matrix(matrix: &Matrix3<f64>) -> Self {
        Self {
            ixx: matrix[(0, 0)],
            ixy: matrix[(0, 1)],
            ixz: matrix[(0, 2)],
            iyy: matrix[(1, 1)],
            iyz: matrix[(1, 2)],
            izz: matrix[(2, 2)],
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MassPropertiesAccumulator {
    mass: f64,
    weighted_center_sum: Vector3<f64>,
    inertia_about_root: Matrix3<f64>,
}

impl MassPropertiesAccumulator {
    pub fn add_inertial(&mut self, inertial: &ResolvedInertial) {
        if inertial.mass <= 0.0 {
            return;
        }

        let center = inertial.origin.translation.vector;
        self.mass += inertial.mass;
        self.weighted_center_sum += center * inertial.mass;
        self.inertia_about_root +=
            inertial.inertia.to_matrix() + Self::parallel_axis_term(inertial.mass, &center);
    }

    pub fn extend(&mut self, other: Self) {
        self.mass += other.mass;
        self.weighted_center_sum += other.weighted_center_sum;
        self.inertia_about_root += other.inertia_about_root;
    }

    pub fn finalize(&self) -> Option<ResolvedInertial> {
        if self.mass <= 0.0 {
            return None;
        }

        let center = self.weighted_center_sum / self.mass;
        let inertia = self.inertia_about_root - Self::parallel_axis_term(self.mass, &center);
        Some(ResolvedInertial {
            origin: Isometry3::translation(center.x, center.y, center.z),
            mass: self.mass,
            inertia: InertiaMatrix::from_matrix(&inertia),
        })
    }

    fn parallel_axis_term(mass: f64, displacement: &Vector3<f64>) -> Matrix3<f64> {
        let distance_squared = displacement.dot(displacement);
        let identity = Matrix3::identity() * distance_squared;
        mass * (identity - displacement * displacement.transpose())
    }
}

pub fn transform_inertial(
    inertial: &urdf_rs::Inertial,
    transform: &Isometry3<f64>,
) -> ResolvedInertial {
    let resolved = ResolvedInertial {
        origin: pose_to_isometry(&inertial.origin),
        mass: inertial.mass.value,
        inertia: InertiaMatrix {
            ixx: inertial.inertia.ixx,
            ixy: inertial.inertia.ixy,
            ixz: inertial.inertia.ixz,
            iyy: inertial.inertia.iyy,
            iyz: inertial.inertia.iyz,
            izz: inertial.inertia.izz,
        },
    };
    let rotation = transform.rotation.to_rotation_matrix();
    let rotated_inertia =
        rotation.matrix() * resolved.inertia.to_matrix() * rotation.matrix().transpose();

    ResolvedInertial {
        origin: transform * resolved.origin,
        mass: resolved.mass,
        inertia: InertiaMatrix::from_matrix(&rotated_inertia),
    }
}

#[cfg(test)]
mod tests {
    use super::{MassPropertiesAccumulator, transform_inertial};

    fn pose(x: f64, y: f64, z: f64) -> urdf_rs::Pose {
        urdf_rs::Pose {
            xyz: urdf_rs::Vec3([x, y, z]),
            rpy: urdf_rs::Vec3([0.0, 0.0, 0.0]),
        }
    }

    fn inertia(ixx: f64, iyy: f64, izz: f64) -> urdf_rs::Inertia {
        urdf_rs::Inertia {
            ixx,
            ixy: 0.0,
            ixz: 0.0,
            iyy,
            iyz: 0.0,
            izz,
        }
    }

    fn inertial(origin: urdf_rs::Pose, mass: f64, inertia: urdf_rs::Inertia) -> urdf_rs::Inertial {
        urdf_rs::Inertial {
            origin,
            mass: urdf_rs::Mass { value: mass },
            inertia,
        }
    }

    #[test]
    fn accumulator_recomputes_center_of_mass_and_inertia() {
        let mut accumulator = MassPropertiesAccumulator::default();
        accumulator.add_inertial(&transform_inertial(
            &inertial(pose(0.0, 0.0, 0.0), 2.0, inertia(1.0, 1.5, 2.0)),
            &nalgebra::Isometry3::identity(),
        ));
        accumulator.add_inertial(&transform_inertial(
            &inertial(pose(0.0, 0.0, 0.0), 1.0, inertia(0.5, 0.5, 0.5)),
            &nalgebra::Isometry3::translation(1.0, 0.0, 0.0),
        ));

        let merged = accumulator.finalize().expect("mass should be positive");
        let center = merged.origin.translation.vector;
        assert!((merged.mass - 3.0).abs() < 1.0e-12);
        assert!((center.x - (1.0 / 3.0)).abs() < 1.0e-12);
        assert!(center.y.abs() < 1.0e-12);
        assert!(center.z.abs() < 1.0e-12);
        assert!((merged.inertia.ixx - 1.5).abs() < 1.0e-12);
        assert!((merged.inertia.iyy - (2.0 / 3.0 + 0.5 + 1.5)).abs() < 1.0e-12);
        assert!((merged.inertia.izz - (2.0 / 3.0 + 0.5 + 2.0)).abs() < 1.0e-12);
    }
}
