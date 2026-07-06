import path from "node:path"
import solidPlugin from "@opentui/solid/bun-plugin"

type BuildArgs = {
  outfile: string
  target?: Bun.Build.CompileTarget
  minify: boolean
  bytecode: boolean
}

const args = parseArgs(Bun.argv.slice(2))
const packageRoot = import.meta.dir
const entrypoint = path.join(packageRoot, "src", "index.tsx")
const compileOptions: CompileBuildOptions = {
  outfile: args.outfile,
  autoloadBunfig: false,
  autoloadDotenv: false,
  autoloadPackageJson: false,
  autoloadTsconfig: false,
}

if (args.target) {
  compileOptions.target = args.target
}

const result = await Bun.build({
  entrypoints: [entrypoint],
  root: packageRoot,
  target: "bun",
  sourcemap: "none",
  minify: args.minify,
  bytecode: args.bytecode,
  plugins: [solidPlugin],
  compile: compileOptions,
  throw: false,
})

if (!result.success) {
  for (const log of result.logs) {
    console.error(log.message)
  }
  process.exit(1)
}

console.log(args.outfile)

function parseArgs(raw: string[]): BuildArgs {
  let outfile = path.resolve(
    process.cwd(),
    process.env.INDUCTOR_TUI_OUTFILE ?? path.join("dist", packagedFrontendName()),
  )
  let target = process.env.INDUCTOR_TUI_TARGET as Bun.Build.CompileTarget | undefined
  let minify = envFlag("INDUCTOR_TUI_MINIFY")
  let bytecode = envFlag("INDUCTOR_TUI_BYTECODE")

  for (let index = 0; index < raw.length; index += 1) {
    const part = raw[index]
    switch (part) {
      case "--outfile": {
        const value = raw[index + 1]
        if (!value) throw new Error("missing value for --outfile")
        outfile = path.resolve(process.cwd(), value)
        index += 1
        break
      }
      case "--target": {
        const value = raw[index + 1]
        if (!value) throw new Error("missing value for --target")
        target = value as Bun.Build.CompileTarget
        index += 1
        break
      }
      case "--minify":
        minify = true
        break
      case "--bytecode":
        bytecode = true
        break
      default:
        throw new Error(`unknown argument: ${part}`)
    }
  }

  return { outfile, target, minify, bytecode }
}

function packagedFrontendName() {
  return process.platform === "win32" ? "inductor-open-tui.exe" : "inductor-open-tui"
}

function envFlag(name: string) {
  const value = process.env[name]
  return value === "1" || value === "true"
}

type CompileBuildOptions = NonNullable<BuildConfig["compile"]> extends infer T
  ? T extends object
    ? T
    : never
  : never