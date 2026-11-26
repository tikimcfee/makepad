//! Procedural 3D hardware meshes (4 instanced draw calls) and orthographic panel projection math.

use makepad_widgets::*;

fn push_tri(verts: &mut Vec<f32>, idx: &mut Vec<u32>, a: Vec3f, b: Vec3f, c: Vec3f) {
    let ab = vec3(b.x - a.x, b.y - a.y, b.z - a.z);
    let ac = vec3(c.x - a.x, c.y - a.y, c.z - a.z);
    let n = vec3(
        ab.y * ac.z - ab.z * ac.y,
        ab.z * ac.x - ab.x * ac.z,
        ab.x * ac.y - ab.y * ac.x,
    );
    let len = (n.x * n.x + n.y * n.y + n.z * n.z).sqrt().max(1e-6);
    let n = vec3(n.x / len, n.y / len, n.z / len);
    let base = (verts.len() / 8) as u32;
    for p in [a, b, c] {
        verts.extend_from_slice(&[p.x, p.y, p.z, 1.0, n.x, n.y, n.z, 0.0]);
    }
    idx.extend_from_slice(&[base, base + 1, base + 2]);
}

fn push_quad(verts: &mut Vec<f32>, idx: &mut Vec<u32>, a: Vec3f, b: Vec3f, c: Vec3f, d: Vec3f, n: Vec3f) {
    let base = (verts.len() / 8) as u32;
    for p in [a, b, c, d] {
        verts.extend_from_slice(&[p.x, p.y, p.z, 1.0, n.x, n.y, n.z, 0.0]);
    }
    idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn append_box_part(verts: &mut Vec<f32>, idx: &mut Vec<u32>, cx: f32, cy: f32, cz: f32, hx: f32, hy: f32, hz: f32) {
    let faces = [
        (vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0)),
        (vec3(-1.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0), vec3(0.0, 1.0, 0.0)),
        (vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0), vec3(1.0, 0.0, 0.0)),
        (vec3(0.0, -1.0, 0.0), vec3(1.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0)),
        (vec3(0.0, 0.0, 1.0), vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0)),
        (vec3(0.0, 0.0, -1.0), vec3(0.0, 1.0, 0.0), vec3(1.0, 0.0, 0.0)),
    ];
    let corners: [(f32, f32); 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    for (normal, au, av) in faces {
        let base = (verts.len() / 8) as u32;
        for (u, v) in corners {
            let p = vec3(
                cx + (normal.x + au.x * u + av.x * v) * hx,
                cy + (normal.y + au.y * u + av.y * v) * hy,
                cz + (normal.z + au.z * u + av.z * v) * hz,
            );
            verts.extend_from_slice(&[p.x, p.y, p.z, 1.0, normal.x, normal.y, normal.z, 0.0]);
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

fn append_cyl_z(verts: &mut Vec<f32>, idx: &mut Vec<u32>, r_inner: f32, r_outer: f32, z0: f32, z1: f32, segs: usize) {
    let tau = std::f32::consts::TAU;
    for i in 0..segs {
        let a0 = (i as f32 / segs as f32) * tau;
        let a1 = ((i + 1) as f32 / segs as f32) * tau;
        let (c0, s0) = (a0.cos(), a0.sin());
        let (c1, s1) = (a1.cos(), a1.sin());
        let base = (verts.len() / 8) as u32;
        verts.extend_from_slice(&[c0 * r_outer, s0 * r_outer, z0, 1.0, c0, s0, 0.0, 0.0]);
        verts.extend_from_slice(&[c1 * r_outer, s1 * r_outer, z0, 1.0, c1, s1, 0.0, 0.0]);
        verts.extend_from_slice(&[c1 * r_outer, s1 * r_outer, z1, 1.0, c1, s1, 0.0, 0.0]);
        verts.extend_from_slice(&[c0 * r_outer, s0 * r_outer, z1, 1.0, c0, s0, 0.0, 0.0]);
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);

        if r_inner > 0.001 {
            let base = (verts.len() / 8) as u32;
            verts.extend_from_slice(&[c1 * r_inner, s1 * r_inner, z0, 1.0, -c1, -s1, 0.0, 0.0]);
            verts.extend_from_slice(&[c0 * r_inner, s0 * r_inner, z0, 1.0, -c0, -s0, 0.0, 0.0]);
            verts.extend_from_slice(&[c0 * r_inner, s0 * r_inner, z1, 1.0, -c0, -s0, 0.0, 0.0]);
            verts.extend_from_slice(&[c1 * r_inner, s1 * r_inner, z1, 1.0, -c1, -s1, 0.0, 0.0]);
            idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);

            push_quad(
                verts, idx,
                vec3(c0 * r_inner, s0 * r_inner, z1),
                vec3(c0 * r_outer, s0 * r_outer, z1),
                vec3(c1 * r_outer, s1 * r_outer, z1),
                vec3(c1 * r_inner, s1 * r_inner, z1),
                vec3(0.0, 0.0, 1.0),
            );
            push_quad(
                verts, idx,
                vec3(c1 * r_inner, s1 * r_inner, z0),
                vec3(c1 * r_outer, s1 * r_outer, z0),
                vec3(c0 * r_outer, s0 * r_outer, z0),
                vec3(c0 * r_inner, s0 * r_inner, z0),
                vec3(0.0, 0.0, -1.0),
            );
        } else {
            push_tri(
                verts, idx,
                vec3(0.0, 0.0, z1),
                vec3(c0 * r_outer, s0 * r_outer, z1),
                vec3(c1 * r_outer, s1 * r_outer, z1),
            );
            push_tri(
                verts, idx,
                vec3(0.0, 0.0, z0),
                vec3(c1 * r_outer, s1 * r_outer, z0),
                vec3(c0 * r_outer, s0 * r_outer, z0),
            );
        }
    }
}

pub fn build_box_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_box_part(&mut verts, &mut idx, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0);
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

pub fn build_cyl_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_cyl_z(&mut verts, &mut idx, 0.0, 1.0, -0.5, 0.5, 24);
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

/// Flat machined aluminum 6-spoke jog wheel (reused from cassette reel geometry).
pub fn build_jog_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_cyl_z(&mut verts, &mut idx, 0.84, 1.00, 0.0, 0.24, 32);
    append_cyl_z(&mut verts, &mut idx, 0.22, 0.42, 0.0, 0.30, 20);
    let tau = std::f32::consts::TAU;
    for i in 0..6 {
        let a = (i as f32 / 6.0) * tau;
        let (ca, sa) = (a.cos(), a.sin());
        let (tx, ty) = (-sa, ca);
        let hw = if i == 0 { 0.10 } else { 0.055 };
        let r0 = 0.38;
        let r1 = 0.88;
        let z0 = 0.04;
        let z1 = if i == 0 { 0.28 } else { 0.22 };
        let p00 = vec3(ca * r0 - tx * hw, sa * r0 - ty * hw, z1);
        let p10 = vec3(ca * r1 - tx * hw, sa * r1 - ty * hw, z1);
        let p11 = vec3(ca * r1 + tx * hw, sa * r1 + ty * hw, z1);
        let p01 = vec3(ca * r0 + tx * hw, sa * r0 + ty * hw, z1);
        let b00 = vec3(p00.x, p00.y, z0);
        let b10 = vec3(p10.x, p10.y, z0);
        let b11 = vec3(p11.x, p11.y, z0);
        let b01 = vec3(p01.x, p01.y, z0);
        push_quad(&mut verts, &mut idx, p00, p10, p11, p01, vec3(0.0, 0.0, 1.0));
        push_quad(&mut verts, &mut idx, b01, b11, b10, b00, vec3(0.0, 0.0, -1.0));
        push_quad(&mut verts, &mut idx, b00, b10, p10, p00, vec3(-tx, -ty, 0.0));
        push_quad(&mut verts, &mut idx, b11, b01, p01, p11, vec3(tx, ty, 0.0));
    }
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

pub fn build_knob_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_cyl_z(&mut verts, &mut idx, 0.0, 1.00, 0.0, 0.22, 24);
    append_cyl_z(&mut verts, &mut idx, 0.0, 0.84, 0.22, 0.86, 20);
    // Crisp high-contrast pointer bar protruding from knob top
    append_box_part(&mut verts, &mut idx, 0.0, 0.44, 0.90, 0.11, 0.36, 0.10);
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

/// Right-handed orthographic projection in world space (+X right, +Y up, +Z toward viewer)
/// using Makepad's [0, 1] clip-space Z convention (Makepad maps `2 * z - w` for OpenGL):
/// `z = z_near (+30)` -> `clip_z = 0.0` (`gl_Position.z = -1.0`),
/// `z = z_far  (-30)` -> `clip_z = 1.0` (`gl_Position.z = +1.0`).
pub fn mat4_ortho_world(left: f32, right: f32, bottom: f32, top: f32, z_far: f32, z_near: f32) -> Mat4f {
    let dx = (right - left).max(1e-4);
    let dy = (top - bottom).max(1e-4);
    let dz = (z_near - z_far).max(1e-4);
    Mat4f {
        v: [
            2.0 / dx, 0.0, 0.0, 0.0,
            0.0, 2.0 / dy, 0.0, 0.0,
            0.0, 0.0, -1.0 / dz, 0.0,
            -(right + left) / dx, -(top + bottom) / dy, z_near / dz, 1.0,
        ],
    }
}

pub fn mat4_trs_z(tx: f32, ty: f32, tz: f32, rot_z: f32, sx: f32, sy: f32, sz: f32) -> Mat4f {
    let (c, s) = (rot_z.cos(), rot_z.sin());
    Mat4f {
        v: [
            c * sx, s * sx, 0.0, 0.0,
            -s * sy, c * sy, 0.0, 0.0,
            0.0, 0.0, sz, 0.0,
            tx, ty, tz, 1.0,
        ],
    }
}
