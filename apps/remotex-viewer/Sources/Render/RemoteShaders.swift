import Foundation

/// The framebuffer blit, as source compiled at runtime.
///
/// A string rather than a `.metal` file because a SwiftPM Swift target cannot
/// compile non-Swift sources — the file would be reported as unhandled and
/// silently excluded. `makeLibrary(source:)` costs tens of milliseconds once at
/// startup and keeps `build-viewer-app.sh` a plain `swift build`.
enum RemoteShaders {
    static let vertexFunction = "remotexBlitVertex"
    static let fragmentFunction = "remotexBlitFragment"

    static let source = """
    #include <metal_stdlib>
    using namespace metal;

    struct Corner {
        float4 position [[position]];
        float2 uv;
    };

    // A four-vertex triangle strip covering the clip volume, generated from the
    // vertex id: no vertex buffer, nothing to bind.
    vertex Corner \(vertexFunction)(uint id [[vertex_id]]) {
        float2 corner = float2((id & 1) ? 1.0 : -1.0, (id & 2) ? 1.0 : -1.0);
        Corner out;
        out.position = float4(corner, 0.0, 1.0);
        // Clip y grows upward; the texture's row 0 is the top of the desktop
        // (see TileDecoder), so v has to run the other way.
        out.uv = float2((corner.x + 1.0) * 0.5, (1.0 - corner.y) * 0.5);
        return out;
    }

    fragment float4 \(fragmentFunction)(
        Corner in [[stage_in]],
        texture2d<float> framebuffer [[texture(0)]],
        sampler nearest [[sampler(0)]]
    ) {
        return framebuffer.sample(nearest, in.uv);
    }
    """
}
