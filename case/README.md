# Project Optic enclosure (Raspberry Pi 5 + HQ Camera)

3D-printable outdoor enclosure for the camera station described in the
[project README](../README.md): a Raspberry Pi 5, the Raspberry Pi High
Quality Camera (IMX477) and the 6 mm f/1.2 CS-mount lens
(PT361060M3MP12).

## Files

| Path | What it is |
| --- | --- |
| `pi5/step/pi-cam-case-assembly.step` | **The source model.** STEP AP214, exported from Fusion on 2026-09-22. One assembly holding all four parts as they fit together. Opens and edits in Fusion, FreeCAD, Onshape, SolidWorks and other CAD tools. |

The four parts in the assembly:

1. **Box base** – holds the Pi 5 and the camera module.
2. **Box lid** – closes the base.
3. **Lens back** – the lens mount behind the aperture.
4. **Lens cover** – the front cover over the lens.

> The bodies are unnamed inside the STEP file (the product name is the
> export timestamp), so other CAD tools show them as generic solids.
> Naming the components in Fusion before the next export would carry the
> names across.

### Not here yet

- **Per-part STEP files.** Exporting each component separately
  (right-click the component in the Fusion browser → *Save As STEP*, or
  export with only that component selected) produces one file per part.
  Using *File → Export* instead writes the whole design every time, which
  is why the first attempt produced four identical files.
- **Print-ready meshes** (`.3mf` preferred over `.stl`: it carries units,
  separate parts and print settings). Export each part laid flat in its
  print orientation, not in assembly position.
- **The Fusion source** (`.f3d`), which is the only format that keeps the
  parametric timeline.
- A dimensioned drawing (PDF) and a photo of the assembled box.

The earlier `RaspberryPi5caseV5T-*.stl` files were removed when this STEP
model replaced them: a mesh cannot be edited meaningfully, and those files
were a different design.

## Printing

Material: **ASA** — chosen for UV and heat resistance in a station meant to
run outdoors for a year. It needs an enclosed printer. PETG is a workable
substitute; PLA is not, since it sags in direct sun and goes brittle.

Everything below is still to be filled in from a real print:

| Setting | Value |
| --- | --- |
| Layer height | TBD |
| Wall loops / perimeters | TBD |
| Infill | TBD |
| Supports | TBD |
| Print orientation per part | TBD |
| Estimated print time / filament | TBD |

## Hardware (bill of materials)

TBD — screws, heat-set inserts, fan (the previous design used a 30 mm
fan), gasket or seal, and any lens-mount hardware.

## Assembly

TBD — the order the parts go together, where the cables run, and how the
camera ribbon is routed.

## Licence

The enclosure design in this directory is licensed under the **CERN Open
Hardware Licence Version 2 – Strongly Reciprocal** (`CERN-OHL-S-2.0`); see
[`LICENSE-hardware.txt`](LICENSE-hardware.txt). In short: you may use,
study, modify, make and distribute it, provided you pass on the same
freedoms and publish the source of any modified design you distribute.

This covers the hardware design files only. The software in this
repository is licensed separately.
