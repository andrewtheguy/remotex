// BETA: the modules this page decodes in software, as it answered at load: a
// Mac's passed HEVC (appleMedia.ts), VP9 at 4:4:4 (videoChroma.ts). The paint
// worker is told the list, and the Info card says of a stream whether it is one
// of them.

import { appleHevcDecoder } from "./appleMedia.ts";
import { type SoftwareModule, softwareModuleFor } from "./softwareDecoder.ts";
import { videoDecoder } from "./videoChroma.ts";

/** The modules this page decodes with, and not with the browser's decoder. */
export function softwareModules(): SoftwareModule[] {
  const modules: SoftwareModule[] = [];
  if (appleHevcDecoder() === "software") {
    modules.push("hevc");
  }
  if (videoDecoder() === "software") {
    modules.push("vp9");
  }
  return modules;
}

/** Whether a stream of this configuration is decoded in one of them here. */
export function decodedInSoftware(decode: string): boolean {
  const module = softwareModuleFor(decode);
  return module !== null && softwareModules().includes(module);
}
