# Proposed ShowMesh integration

Status: proposal. Nothing here is implemented in the ShowMesh coordinator, and nothing in this repository depends on it.

## Principles

- The receiver is a standalone device. It plays its saved configuration whether or not the coordinator is reachable, and a coordinator outage never stops video.
- The coordinator talks to the receiver only through the public REST API in [api/openapi.yaml](../api/openapi.yaml). The receiver does not join the ShowMesh MQTT broker and does not call the coordinator.
- Desired and observed state stay separate, as in the rest of ShowMesh. An assignment the coordinator sends is desired state. The receiver's `GET /api/v1/health` report is the observation; an assignment has worked only when that report shows the output `playing` after the change.

## Proposed flow

1. **Registration.** An operator adds a receiver to ShowMesh by its address (`http://host:8790`). The coordinator reads `GET /api/v1/device` to confirm it is a `showmesh-ndi` device and to record its name and version.
2. **Observation.** The coordinator polls `GET /api/v1/health` every few seconds and `GET /api/v1/outputs` when it needs modes or assignments. A receiver that stops answering is shown as unreachable; its outputs keep playing.
3. **Control.** Assigning a source is `PUT /api/v1/outputs/{name}/source`. Mode changes, start, stop and restart use the matching routes. The coordinator then checks the next health report.
4. **Sources.** `GET /api/v1/sources` lists the NDI sources this receiver can see, which may differ between receivers on different network segments.

## Open questions for the ShowMesh side

- Automatic discovery. A receiver could advertise `_showmesh-ndi._tcp` over mDNS so the coordinator finds it without an address. Not implemented.
- Authentication. The API is open, matching the dedicated show network assumption. If the coordinator needs the receiver to reject other callers, the companion token pattern (ADR-059 in the ShowMesh repository) is the likely model.
- Where receiver state appears in the operator UI, and whether `showmeshctl` gains matching commands. These belong to the ShowMesh repository and need its own design and decision records.
