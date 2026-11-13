# Renders assets/icon.png with Blender (tested with 4.2 LTS):
#   blender -b -P assets/icon-source/still_icon.py -- out.png 1024 256
# then: magick out.png -strip PNG32:assets/icon.png (and icon-256.png / icon.ico, see README).
# FONT below points at Noto Sans Black; any heavy sans works.

# Still app icon: a mirror-polished steel ball with an S carved into it.
# Run: blender -b -P icon_scene.py -- <out.png> <size> <samples>
import bpy, math, sys

argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
OUT = argv[0] if len(argv) > 0 else "/tmp/icon.png"
SIZE = int(argv[1]) if len(argv) > 1 else 1024
SAMPLES = int(argv[2]) if len(argv) > 2 else 256
FONT = "/usr/share/fonts/noto/NotoSans-Black.ttf"
HDRI = bpy.utils.resource_path("LOCAL") + "/datafiles/studiolights/world/studio.exr"
DEPTH = 0.12  # groove depth, in ball radii

bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene

# ---------------------------------------------------------------- materials
def metal(name, color, rough, aniso=0.0):
    m = bpy.data.materials.new(name)
    m.use_nodes = True
    b = m.node_tree.nodes["Principled BSDF"]
    b.inputs["Base Color"].default_value = (*color, 1.0)
    b.inputs["Metallic"].default_value = 1.0
    b.inputs["Roughness"].default_value = rough
    if aniso:
        b.inputs["Anisotropic"].default_value = aniso
    return m

chrome = metal("Chrome", (0.93, 0.93, 0.95), 0.045)
groove = metal("Groove", (0.045, 0.046, 0.05), 0.42)

# ---------------------------------------------------------------- ball
bpy.ops.mesh.primitive_ico_sphere_add(subdivisions=7, radius=1.0)
ball = bpy.context.active_object
ball.name = "Ball"
ball.data.materials.append(chrome)

# ---------------------------------------------------------------- the S
bpy.ops.object.text_add()
txt = bpy.context.active_object
txt.data.body = "S"
txt.data.font = bpy.data.fonts.load(FONT)
txt.data.align_x = "CENTER"
txt.data.align_y = "CENTER"
txt.data.size = 2.05
txt.data.extrude = 0.7
txt.data.resolution_u = 32
CAM_Z = 2.2
tilt = math.atan2(CAM_Z, 9.0)
# the letter faces the camera, which looks down on the ball a little
txt.rotation_euler = (math.radians(90) - tilt, 0, 0)
txt.location = (0.0, -1.1 * math.cos(tilt), 1.1 * math.sin(tilt))
bpy.ops.object.convert(target="MESH")
cutter = bpy.context.active_object
cutter.data.materials.clear()
cutter.data.materials.append(groove)

# keep only the part of the letter that lies within DEPTH of the surface,
# so the groove floor follows the curve of the ball
bpy.ops.mesh.primitive_ico_sphere_add(subdivisions=7, radius=1.0 - DEPTH)
inner = bpy.context.active_object
inner.data.materials.append(groove)

def apply_bool(target, other, op):
    bpy.context.view_layer.objects.active = target
    mod = target.modifiers.new("b", "BOOLEAN")
    mod.operation = op
    mod.solver = "EXACT"
    mod.object = other
    if hasattr(mod, "material_mode"):
        mod.material_mode = "TRANSFER"
    bpy.ops.object.modifier_apply(modifier=mod.name)

apply_bool(cutter, inner, "DIFFERENCE")
apply_bool(ball, cutter, "DIFFERENCE")
for o in (cutter, inner):
    bpy.data.objects.remove(o, do_unlink=True)

bpy.context.view_layer.objects.active = ball
ball.select_set(True)
bpy.ops.object.shade_smooth_by_angle(angle=math.radians(35))
# a hair of bevel on the groove lip catches a bright rim of light
bev = ball.modifiers.new("lip", "BEVEL")
bev.width = 0.006
bev.segments = 3
bev.limit_method = "ANGLE"
bev.angle_limit = math.radians(35)
bev.harden_normals = True

# ---------------------------------------------------------------- light
world = bpy.data.worlds.new("World")
scene.world = world
world.use_nodes = True
nt = world.node_tree
env = nt.nodes.new("ShaderNodeTexEnvironment")
env.image = bpy.data.images.load(HDRI)
mapping = nt.nodes.new("ShaderNodeMapping")
coord = nt.nodes.new("ShaderNodeTexCoord")
mapping.inputs["Rotation"].default_value = (0, 0, math.radians(200))
nt.links.new(coord.outputs["Generated"], mapping.inputs["Vector"])
nt.links.new(mapping.outputs["Vector"], env.inputs["Vector"])
bg = nt.nodes["Background"]
bg.inputs["Strength"].default_value = 1.0
# a studio horizon: dark floor, a bright band at the horizon, a soft sky
dirn = nt.nodes.new("ShaderNodeSeparateXYZ")
nt.links.new(coord.outputs["Generated"], dirn.inputs["Vector"])
ramp = nt.nodes.new("ShaderNodeValToRGB")
els = ramp.color_ramp.elements
els[0].position = 0.0
els[0].color = (0.30, 0.30, 0.32, 1)
els[1].position = 1.0
els[1].color = (0.72, 0.74, 0.78, 1)
for pos, c in [(0.46, (0.42, 0.42, 0.44, 1)), (0.49, (0.025, 0.025, 0.03, 1)), (0.515, (0.025, 0.025, 0.03, 1)),
               (0.535, (2.4, 2.4, 2.5, 1)), (0.62, (1.15, 1.17, 1.22, 1)), (0.82, (0.85, 0.87, 0.92, 1))]:
    e = els.new(pos)
    e.color = c
remap = nt.nodes.new("ShaderNodeMapRange")
remap.inputs["From Min"].default_value = -1.0
remap.inputs["From Max"].default_value = 1.0
nt.links.new(dirn.outputs["Z"], remap.inputs["Value"])
nt.links.new(remap.outputs["Result"], ramp.inputs["Fac"])
mix = nt.nodes.new("ShaderNodeMix")
mix.data_type = "RGBA"
mix.blend_type = "MULTIPLY"
mix.inputs["Factor"].default_value = 0.45
nt.links.new(ramp.outputs["Color"], mix.inputs["A"])
nt.links.new(env.outputs["Color"], mix.inputs["B"])
nt.links.new(mix.outputs["Result"], bg.inputs["Color"])

def softbox(name, loc, rot, size, power, color=(1, 1, 1)):
    bpy.ops.object.light_add(type="AREA", location=loc, rotation=rot)
    l = bpy.context.active_object
    l.name = name
    l.data.shape = "RECTANGLE"
    l.data.size, l.data.size_y = size
    l.data.energy = power
    l.data.color = color
    l.visible_camera = False
    return l

# big key strip up-left, a rim on the right, a low fill: the ball mirrors them
softbox("Key", (-3.2, -3.0, 3.6), (math.radians(50), 0, math.radians(-45)), (1.2, 5.0), 900)
softbox("Rim", (3.6, 1.0, 1.2), (math.radians(90), 0, math.radians(105)), (0.7, 4.0), 520, (0.85, 0.92, 1.0))
softbox("Fill", (0.5, -4.0, -0.2), (math.radians(92), 0, 0), (5.0, 0.6), 160)

# ---------------------------------------------------------------- camera
bpy.ops.object.camera_add(location=(0, -9.0, CAM_Z))
cam = bpy.context.active_object
cam.data.lens = 134
cam.data.shift_y = 0.0
direction = -cam.location
cam.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()
scene.camera = cam

# ---------------------------------------------------------------- render
r = scene.render
scene.render.engine = "CYCLES"
scene.cycles.device = "CPU"
scene.cycles.samples = SAMPLES
scene.cycles.use_denoising = True
scene.cycles.max_bounces = 12
scene.cycles.glossy_bounces = 12
r.resolution_x = r.resolution_y = SIZE
r.resolution_percentage = 100
r.film_transparent = True
r.image_settings.file_format = "PNG"
r.image_settings.color_mode = "RGBA"
r.image_settings.color_depth = "16"
scene.view_settings.view_transform = "AgX"
scene.view_settings.look = "AgX - Punchy"
r.filepath = OUT
bpy.ops.render.render(write_still=True)
print("WROTE", OUT)
