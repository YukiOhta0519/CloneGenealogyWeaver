# CLAIM のプラグインの入口．
#
# 引数が --versions なら Python と依存の版を，そうでなければ CLAIM の build context を，JSON で標準出力に書く．
import json
import sys

PACKAGES = ("dockerfile", "PyYAML", "python-dotenv", "parameter-expansion-patched")


def versions():
    import platform
    from importlib.metadata import version

    return {
        "python": platform.python_version(),
        "packages": {name: version(name) for name in PACKAGES},
    }


# context を実在するディレクトリに対応づけないのは，Rust の側が作業ツリーの外を見ない決まりで対応づけるからである．
def contexts(name, workdir):
    from claim import claim

    return sorted(
        {
            service.build.context
            for service in claim(name, workdir)
            if service.build is not None and service.build.context is not None
        }
    )


if __name__ == "__main__":
    if sys.argv[1:] == ["--versions"]:
        json.dump(versions(), sys.stdout)
    else:
        name, workdir = sys.argv[1:]
        json.dump({"contexts": contexts(name, workdir)}, sys.stdout)
