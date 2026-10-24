"""Converts Real-ESRGAN weights (BSD-3, github.com/xinntao/Real-ESRGAN) to ONNX for Grainhouse.

Usage: esrgan_to_onnx.py <weights.pth> <out.onnx>
RealESRGAN_x4plus.pth is an RRDBNet; realesr-general-*x4v3.pth are SRVGGNetCompact. The networks are written out
here (same layer names as Real-ESRGAN/BasicSR) so only torch is needed. Input and output are float32 RGB in
0..1, NCHW, any height and width. The script checks ONNX Runtime against PyTorch before finishing.
"""
import sys

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F


class ResidualDenseBlock(nn.Module):
    def __init__(self, nf=64, gc=32):
        super().__init__()
        self.conv1 = nn.Conv2d(nf, gc, 3, 1, 1)
        self.conv2 = nn.Conv2d(nf + gc, gc, 3, 1, 1)
        self.conv3 = nn.Conv2d(nf + 2 * gc, gc, 3, 1, 1)
        self.conv4 = nn.Conv2d(nf + 3 * gc, gc, 3, 1, 1)
        self.conv5 = nn.Conv2d(nf + 4 * gc, nf, 3, 1, 1)
        self.lrelu = nn.LeakyReLU(0.2, inplace=True)

    def forward(self, x):
        x1 = self.lrelu(self.conv1(x))
        x2 = self.lrelu(self.conv2(torch.cat((x, x1), 1)))
        x3 = self.lrelu(self.conv3(torch.cat((x, x1, x2), 1)))
        x4 = self.lrelu(self.conv4(torch.cat((x, x1, x2, x3), 1)))
        x5 = self.conv5(torch.cat((x, x1, x2, x3, x4), 1))
        return x5 * 0.2 + x


class RRDB(nn.Module):
    def __init__(self, nf, gc=32):
        super().__init__()
        self.rdb1 = ResidualDenseBlock(nf, gc)
        self.rdb2 = ResidualDenseBlock(nf, gc)
        self.rdb3 = ResidualDenseBlock(nf, gc)

    def forward(self, x):
        return self.rdb3(self.rdb2(self.rdb1(x))) * 0.2 + x


class RRDBNet(nn.Module):
    def __init__(self, nf=64, nb=23, gc=32):
        super().__init__()
        self.conv_first = nn.Conv2d(3, nf, 3, 1, 1)
        self.body = nn.Sequential(*[RRDB(nf, gc) for _ in range(nb)])
        self.conv_body = nn.Conv2d(nf, nf, 3, 1, 1)
        self.conv_up1 = nn.Conv2d(nf, nf, 3, 1, 1)
        self.conv_up2 = nn.Conv2d(nf, nf, 3, 1, 1)
        self.conv_hr = nn.Conv2d(nf, nf, 3, 1, 1)
        self.conv_last = nn.Conv2d(nf, 3, 3, 1, 1)
        self.lrelu = nn.LeakyReLU(0.2, inplace=True)

    def forward(self, x):
        feat = self.conv_first(x)
        feat = feat + self.conv_body(self.body(feat))
        feat = self.lrelu(self.conv_up1(F.interpolate(feat, scale_factor=2, mode="nearest")))
        feat = self.lrelu(self.conv_up2(F.interpolate(feat, scale_factor=2, mode="nearest")))
        return self.conv_last(self.lrelu(self.conv_hr(feat)))


class SRVGGNetCompact(nn.Module):
    def __init__(self, nf=64, nconv=32, upscale=4):
        super().__init__()
        self.upscale = upscale
        self.body = nn.ModuleList([nn.Conv2d(3, nf, 3, 1, 1), nn.PReLU(num_parameters=nf)])
        for _ in range(nconv):
            self.body.append(nn.Conv2d(nf, nf, 3, 1, 1))
            self.body.append(nn.PReLU(num_parameters=nf))
        self.body.append(nn.Conv2d(nf, 3 * upscale * upscale, 3, 1, 1))
        self.upsampler = nn.PixelShuffle(upscale)

    def forward(self, x):
        out = x
        for layer in self.body:
            out = layer(out)
        out = self.upsampler(out)
        return out + F.interpolate(x, scale_factor=self.upscale, mode="nearest")


class Clamped(nn.Module):
    def __init__(self, net):
        super().__init__()
        self.net = net

    def forward(self, x):
        return self.net(x).clamp(0, 1)


def main():
    src, dst = sys.argv[1], sys.argv[2]
    sd = torch.load(src, map_location="cpu", weights_only=True)
    sd = sd.get("params_ema", sd.get("params", sd))
    net = RRDBNet() if any(k.startswith("conv_first") for k in sd) else SRVGGNetCompact()
    net.load_state_dict(sd, strict=True)
    model = Clamped(net).eval()
    x = torch.rand(1, 3, 64, 48)
    torch.onnx.export(model, x, dst, input_names=["input"], output_names=["output"], opset_version=17,
                      dynamic_axes={"input": {2: "h", 3: "w"}, "output": {2: "h4", 3: "w4"}}, dynamo=False)
    import onnxruntime as ort
    sess = ort.InferenceSession(dst, providers=["CPUExecutionProvider"])
    y = torch.rand(1, 3, 40, 56)
    with torch.no_grad():
        want = model(y).numpy()
    got = sess.run(None, {"input": y.numpy()})[0]
    print(f"{type(net).__name__}: output {got.shape}, max difference from torch {np.abs(got - want).max():.2e}")


main()
