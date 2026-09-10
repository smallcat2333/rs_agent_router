"""将用户原始 PNG 转成 EXE、窗口与托盘图标；仅转换尺寸/格式，不修改图案。"""
from pathlib import Path
import argparse
import shutil
from PIL import Image


def main():
    """接收源 PNG 路径，在本脚本目录保留原图并生成 Windows 所需资产。"""
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    args = parser.parse_args()
    target = Path(__file__).resolve().parent
    source = args.source.resolve()
    if source != target / "app.png":
        shutil.copyfile(str(source), str(target / "app.png"))
    with Image.open(str(source)) as image:
        rgba = image.convert("RGBA")
        rgba.save(str(target / "app.ico"), format="ICO", sizes=[(n, n) for n in (16, 24, 32, 48, 64, 128, 256)])
        for size, name in ((32, "tray.png"), (256, "window.png")):
            rgba.resize((size, size), Image.Resampling.LANCZOS).save(str(target / name))


if __name__ == "__main__":
    main()
