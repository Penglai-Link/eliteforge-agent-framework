// Resolve publisher services from the running CLI, including desktop app.asar.
// Temporary plugins cannot resolve bare packages relative to their own directory.
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
export const requireDsh = createRequire(process.argv[1]);
export const loadDsh = name => import(pathToFileURL(requireDsh.resolve(name)).href);
export const bindingType = "plugin:eliteforge-external/workspace";
export function bindingOf(events) {
  return events.findLast(event => event.type === bindingType && event.ignorable === true)?.data;
}
