# GPU skinning — the client-side change

`.notes/perf.md` (the 2026-09-16 GPU-skinning entry) has the engine mechanism and
how it is verified; `frame-cost-profile.md` §7.1 item 4 has the cost attribution.
This is what a *scene* does to use it.

## The shape of the change

The animation loop does not change. `rl.updateModelAnimation` is still the call
that advances a model — in a `gpu-skinning` build it only fills `currentPose` and
`boneMatrices`, because the loader never allocated `animVertices` for it to
deform. What changes is where those matrices go: `rl.drawModelEx` uploads them to
the material's shader, so every shader that draws an animated mesh has to skin
it. So the migration is one routing call per model at load, plus the skinning in
the shaders.

## 1. The shaders

Raylib binds the canonical attribute names to fixed locations before it links a
shader, and looks the matrix uniform up by name, so the inputs are not free-form:

| input | where it comes from |
| --- | --- |
| `vec4 vertexBoneIndices` | attribute location 7; four **unnormalised** `unsigned char`s that GL widens to floats (values 0..255) |
| `vec4 vertexBoneWeights` | attribute location 8; four floats |
| `uniform mat4 boneMatrices[N]` | `DrawModelEx`, uploading `N` = the model's bone count |

Two traps in that table. `vertexBoneIndices` has to be `vec4` and cast with
`int()`: raylib sets the attribute up with `glVertexAttribPointer`, not
`glVertexAttribIPointer`, so an `ivec4` reads garbage. And the declared array
size must be **at least** `rl.modelBoneCount(model)` — `DrawModelEx` uploads
exactly that many, and `glUniformMatrix4fv` with a count larger than the declared
array is an `INVALID_OPERATION` that uploads nothing at all, which draws every
vertex of the mesh through the zero matrix. Size it for the largest rig in the
scene.

The skinning is the usual weighted sum. It happens in model space, so the
transforms the shader already applies apply to the skinned position unchanged —
which is the whole edit against the scene's lit shader: three inputs, one
uniform, the skin matrix, and `skinned` in place of `vertexPosition` everywhere
it is used.

```glsl
#version 330
in vec3 vertexPosition;
in vec2 vertexTexCoord;
in vec3 vertexNormal;
in vec4 vertexColor;
in vec4 vertexBoneIndices;
in vec4 vertexBoneWeights;
uniform mat4 mvp;
uniform mat4 matModel;
uniform mat4 matNormal;
uniform mat4 boneMatrices[64];   // >= the largest bone count in the scene
out vec2 fragTexCoord;
out vec4 fragColor;
out vec3 fragWorldPos;
out vec3 fragNormal;

mat4 skinMatrix() {
    return boneMatrices[int(vertexBoneIndices.x)] * vertexBoneWeights.x
         + boneMatrices[int(vertexBoneIndices.y)] * vertexBoneWeights.y
         + boneMatrices[int(vertexBoneIndices.z)] * vertexBoneWeights.z
         + boneMatrices[int(vertexBoneIndices.w)] * vertexBoneWeights.w;
}

void main() {
    mat4 skin = skinMatrix();
    vec4 skinned = skin * vec4(vertexPosition, 1.0);

    vec4 world = matModel * skinned;
    fragWorldPos = world.xyz;
    fragNormal = normalize(mat3(matNormal) * mat3(skin) * vertexNormal);
    fragTexCoord = vertexTexCoord;
    fragColor = vertexColor;
    gl_Position = mvp * skinned;
}
```

Three notes on that.

- The uniform names are raylib's own — `mvp`, `matModel`, `matNormal` are what it
  looks `SHADER_LOC_MATRIX_MVP`/`_MODEL`/`_NORMAL` up by — so nothing has to be
  renamed. A shader that derives its transforms differently just applies them to
  `skinned` instead of `vertexPosition`; both uses move, the clip position and
  the world position.
- `mat3(skin)` on the normal is not an approximation of the CPU build. The CPU
  pass transforms each normal by `transpose(invert(boneMatrices[i]))`, and for a
  rigid bone matrix the 3x3 of `transpose(invert(M))` *is* the 3x3 of `M` — so
  the weighted sums agree and the shading matches a CPU-skinning build exactly.
  It diverges only if a bone's bind pose and current pose differ by a
  non-uniform scale (a uniform one cancels in `invert(bind) * current`).
- `vertexColor` is not skinned: there is no per-bone colour.

**Every** shader that draws an animated mesh needs those declarations, and the
scene has two more that do: the projected blob shadow (`SHADOW_VS`) and the
shadow-map depth pass (`DEPTH_VS`). Both are a single expression's change, because
both already build their clip position from `matModel * vertexPosition`:

```glsl
// SHADOW_VS, skinned. The world position carries the pose, so the ground
// projection and the `world.y > groundY` test follow it for free — the blob a
// raised hoof casts moves with the hoof.
in vec4 vertexBoneIndices;
in vec4 vertexBoneWeights;
uniform mat4 boneMatrices[64];
// skinMatrix() as above

void main() {
    vec3 world = (matModel * skinMatrix() * vec4(vertexPosition, 1.0)).xyz;
    ...unchanged...
}

// DEPTH_VS, skinned. Skinning is model space, so it goes exactly where
// vertexPosition was: after matModel's operands and before lightVP.
in vec4 vertexBoneIndices;
in vec4 vertexBoneWeights;
uniform mat4 boneMatrices[64];
// skinMatrix() as above

void main() {
    vClip = lightVP * matModel * skinMatrix() * vec4(vertexPosition, 1.0);
    gl_Position = vClip;
}
```

Both fragment shaders are unchanged; neither sees a normal.

The blob shadow is the one that used to *hide* this. With CPU skinning the mesh
arrived already deformed, so a shader carrying no bone data still threw the right
silhouette; with GPU skinning the mesh arrives at the bind pose, and an unskinned
shadow shader throws the same silhouette for every instance — a crowd of
identical shadows rather than a wrong number.

A caveat on the WebGL build: raylib uploads the matrices with `transpose = true`
on `GRAPHICS_API_OPENGL_33` and `false` on `GRAPHICS_API_OPENGL_ES2` (WebGL
cannot transpose there), so `boneMatrices[i] * vec4` is right on desktop and
transposed on WebGL.

## 1b. Never route geometry without bone data through the skinned shader

This is the sharpest trap in the migration, and the scene has two kinds of
geometry that would hit it: the immediate-mode grass, and any model that is not
skinned.

**The grass arrives with world-space vertices.** `renderShadowMap` opens the pass
with `beginShaderMode(depthShader)`, which is only `rlSetShader(shader.id,
shader.locs)` (`rcore.c:1070`), and `drawTufts` then emits one `rl.drawCube` per
tuft. `DrawCube` is `rlPushMatrix(); rlTranslatef(position);
rlBegin(RL_TRIANGLES)` with 36 `rlVertex3f` calls (`rmodels.c:270`); `rlPushMatrix`
sets `RLGL.State.transformRequired = true` and retargets the current matrix to
`RLGL.State.transform` (`rlgl.h:1236`), `rlVertex3f` bakes that transform into the
vertex on the CPU (`rlgl.h:1520`), and `rlPopMatrix` restores it — to identity,
once the stack empties in MODELVIEW mode (`rlgl.h:1251`). So `vertexPosition`
reaches the shader already in world space.

**The batch supplies `matModel` per *flush*, not per draw call.** It comes from
`RLGL.State.transform` at flush time (`rlgl.h:3082`), alongside
`mvp = modelview * projection` — no model, because the vertices are already
placed. And `rlSetShader` flushes *before* it swaps the id (`rlgl.h:4603`), so the
grass batch goes out at the `endShaderMode()` that closes the pass, with every
cube already popped and `transform` back at identity. `lightVP * identity *
world` is exactly right, which is why the grass lands correctly in the shadow map
today. (The herd's mesh is drawn immediately rather than batched — `DrawMesh`
issues its own `rlDrawVertexArrayElements` at `rmodels.c:1655` and never calls
`rlDrawRenderBatch` — so the two paths do not contend for the same `matModel`.)

**What skinning would do to it.** A batch vertex has no bone attributes, so the
shader reads the generic values: indices `(0,0,0,0)`, and weights `(0,0,0,1)`,
because raylib's zeroed default for the weights passes a count of 2 with a `vec4`
type — `rlSetVertexAttributeDefault` takes its "type not recognized" path, logs a
warning and sets nothing (`rmodels.c:1412`, `rlgl.h:4533`), so OpenGL's own
generic default survives. The sum is 1 on bone 0, and nothing uploads
`boneMatrices` on the batch path at all — only the model path does — so
`skinMatrix()` resolves to the *previous frame's* goat bone 0. The grass is
transformed by it: the tell is grass shadow depth that shifts as the goat
animates, plus a first frame where the uniform is still zero and the grass
collapses through `gl_Position.w == 0`. A guard
(`dot(vertexBoneWeights, vec4(1.0)) > 0.0`) cannot see it, because the fallback
sums to exactly 1.

**The model path is worse, not better.** `DrawMesh` re-enables the bone
attributes from the *mesh's own* buffers (`rmodels.c:1619`), so `setModelShader`
on a model without bone data — a `makeModel` mesh, a static prop — binds
attributes 7 and 8 to `vboId[7]`/`vboId[8]`, which are zero for a mesh raylib
never uploaded bone data for. That is undefined attribute reads, not a stale
matrix.

So the rule is per model and only for models that animate: `setModelShader` on
the herd and the player, the plain program for everything else — the grass
included, in all three families (lit, blob shadow, depth). Two programs from one
source is the tidy form: keep the skinning block as its own string and compile
the variant you need. The `SHADER: Failed to set attrib default value, data type
not recognized` line is the symptom to look for in the log.

## 1c. Unrelated, and load-bearing: the grass batch is only correct below ~910 cubes

The mechanism above has a second trip point, and it is not about skinning at
all. The grass's `matModel` is right only because the flush that carries it
happens after every cube has popped; it is not right by construction.

`RL_DEFAULT_BATCH_BUFFER_ELEMENTS` is 8192 and a batch buffer holds
`elementCount * 4` vertices, i.e. 32,768 (`rlgl.h:204`, and the fill test at
`rlgl.h:1537`). A `DrawCube` pushes 36 vertices — 12 triangles in `RL_TRIANGLES`,
not the 24 quads would suggest — so a run of grass flushes itself at about 910
cubes. Past that the auto-flush inside `rlVertex3f` uploads `matModel` from
`transform` *while a cube is still translated*, and the depth shader applies that
translation a second time to the vertices, which are already world space.

The same mechanism makes the grass's correctness depend on the pass *ending* with
a shader change: `rlSetShader` flushes only when the id actually differs
(`rlgl.h:4600`), so a pass that drew grass and then left the depth shader bound
would carry the batch into whichever flush comes next, where `transform` need not
be identity.

| pass | cubes | vertices | of 32,768 |
| --- | ---: | ---: | --- |
| shadow (`shadowGrassCull2()`, ~9-unit radius) | ~30–60 | ~1–2k | comfortable |
| visible (`576`, 24-unit radius) | ~270 | ~10k | comfortable |
| the trip point | ~910 | 32,760 | — |

In the **lit** pass the same overflow corrupts `fragWorldPos`, which feeds both
the shadow-map lookup and the specular view vector, while `gl_Position` stays
correct — `LIT_VS` gets that from `mvp` applied to the CPU-transformed vertex. So
the failure mode to watch if anyone raises the grass field's `half` is geometry
that looks right while the grass's own shadow lookup and speculars drift; the
depth pass fails differently, displacing one partial batch's grass behind the
herd.

## 2. Load: one routing call per model

```js
const skinned = rl.loadShaderFromMemory(VS, FS);  // once, behind rl.GPU_SKINNING
rl.setModelShader(model, skinned);                // per animated model
```

`setModelShader` writes the shader into every material of the model, capturing
the originals the first time so `setModelShader(model, -1)` puts them back.
`drawModelEx` binds `material.shader`, which is also why a shader entered with
`beginShaderMode` cannot route a model — the model path ignores it.

## 3. Per frame: unchanged, but read the sharing rule

```js
rl.updateModelAnimation(model, animIndex, frame);
rl.drawModelEx(model, x, y, z, axisX, axisY, axisZ, angle, sx, sy, sz, tint);
```

That order still matters: the first call fills the bone matrices, the second
uploads them. What the flag buys is that the first call is now the bone loop
instead of a full vertex deform plus two buffer uploads.

It also relaxes a constraint the CPU path imposed. The mesh is no longer
per-instance state, so **one model can serve every instance** — as long as each
instance is updated immediately before it is drawn. Update all seven and then
draw all seven, and all seven draw the last pose, because the pose lives in the
model. The same goes for `modelBoneTransform`/`modelBonePosition`: on a shared
model they describe whichever instance was updated last.

## 4. The fallback for a model you cannot route

```js
if (!rl.isShaderValid(skinned)) {
    rl.setModelShader(model, -1);         // back to the shader it loaded with
    rl.setModelCpuSkinning(model, true);  // and back to the CPU deform pass
}
```

`rl.setModelCpuSkinning(model, true)` allocates that model's deform buffers
again, seeded from the bind pose, so raylib's CPU pass picks the mesh up and the
model draws correctly through a default shader. It is per model and reversible;
`false` hands the buffers back and is refused in a build without the flag.

Branch on `rl.GPU_SKINNING` rather than on behaviour. A fresh model in a
`gpu-skinning` build has no deform buffers, so it must go through a skinned
shader or be given them back; a fresh model in a default build has them, and
routing *that* one through a skinned shader draws garbage, because the bone
attributes are never uploaded there and the shader reads zeros. A scene shipping
one source for both builds should compile the skinned shader only when
`rl.GPU_SKINNING` is true and leave everything else on the default path — which
also means the `setModelShader` call above is inside that branch.

## 5. What this does not cover

- **No per-instance bone matrices.** `boneMatrices` lives in the model, so a herd
  is one `updateModelAnimation` + one `drawModelEx` per instance, in that order
  (§3). There is no binding that takes one instance's matrix array, and the
  single-matrix `setShaderValueMatrix` would be overwritten by the next
  `drawModelEx` anyway.
- **No number from the engine side.** The engine repo ships no skinned model
  asset and both `loadModel` and the CPU deform path need a live GL context, so
  nothing there can time this. The phases to re-measure are `bots` and
  `goat_pose` — and `shadow_bots`, which draws the same meshes through the shadow
  shader and is the cheapest place for a missing skinning path to show itself.
- **Not a model-count win by itself.** One model *can* now serve the whole herd
  (§3), but only if the scene interleaves update and draw; keeping one model per
  goat still works and only loses the CPU deform cost.
- Non-animated models on the default shader, immediate-mode geometry and the 2D
  surface are unaffected by the flag. §1b is about deliberately drawing those
  through the skinned shader, which is a different mistake.

## 6. Checklist

- [ ] Add the three inputs and the skin matrix to every shader that routes to an
      animated model — the lit one, the blob shadow and the depth pass. The
      fragment shaders are untouched.
- [ ] Keep the *plain* programs for the grass and any non-skinned model, in all
      three families (§1b) — the herd's meshes are the only geometry that gets the
      skinned ones.
- [ ] Leave the grass field's `half` under the batch trip point, or move the
      grass to a mesh (§1c: ~910 cubes per batch).
- [ ] Size `boneMatrices[]` for the largest rig in the scene, not for the goat.
- [ ] Compile the skinned shader only when `rl.GPU_SKINNING` is true; keep the
      default-shader path for the other build.
- [ ] Route each animated model with `setModelShader` at load.
- [ ] Route it to those models only: not a static model, not a block of
      immediate-mode geometry (§1b).
- [ ] Fall back per model (`setModelShader(model, -1)` +
      `setModelCpuSkinning(model, true)`) for anything a mod loaded or any shader
      that failed to compile.
- [ ] Check the shadow pass with the herd animating before reading any frame
      number.
- [ ] Re-measure `bots`, `goat_pose` and `shadow_bots` on the client's own frame.
