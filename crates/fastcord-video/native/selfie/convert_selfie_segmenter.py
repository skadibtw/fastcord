#!/usr/bin/env python3
"""Converts MediaPipe's selfie_segmenter.tflite into selfie_segmenter.onnx.

The runtime that fastcord already ships for DeepFilterNet (tract) reads ONNX, not
TFLite, so the original model is translated once, offline, operator by operator.
Weights are copied unchanged (float16 values widened to float32); layout becomes
NCHW; nothing is retrained or quantized. The 11 operator kinds in this one model
are the only ones supported.

    python -m venv venv && venv/bin/pip install tflite onnx numpy
    python convert_selfie_segmenter.py selfie_segmenter.tflite selfie_segmenter.onnx

The result is deterministic for the pinned input (see selfie-model.json).
"""
import sys

import numpy as np
import onnx
import tflite
from onnx import TensorProto, helper, numpy_helper

ACT_NONE, ACT_RELU = 0, 1


def main(src, dst):
    model = tflite.Model.GetRootAsModel(open(src, "rb").read(), 0)
    graph = model.Subgraphs(0)
    opnames = {v: k for k, v in vars(tflite.BuiltinOperator).items() if not k.startswith("_")}

    def shape(i):
        return [int(x) for x in graph.Tensors(i).ShapeAsNumpy()]

    def const(i):
        t = graph.Tensors(i)
        buf = model.Buffers(t.Buffer())
        if buf.DataLength() == 0:
            return None
        dtype = {0: np.float32, 1: np.float16, 2: np.int32}[t.Type()]
        return np.frombuffer(buf.DataAsNumpy().tobytes(), dtype).reshape(shape(i))

    consts = {}  # tensor index -> float32/int32 constant
    nodes, inits = [], []

    def name(i):
        return f"t{i}"

    def init(array, label):
        inits.append(numpy_helper.from_array(np.ascontiguousarray(array), label))
        return label

    def weight(i, perm=None):
        w = consts[i] if i in consts else const(i).astype(np.float32)
        return w.transpose(perm) if perm else w

    def same_pads(inp, out, k, s):
        pads = []
        for n_in, n_out, kk, ss in zip(inp, out, k, s):
            total = max((n_out - 1) * ss + kk - n_in, 0)
            pads.append((total // 2, total - total // 2))
        return [pads[0][0], pads[1][0], pads[0][1], pads[1][1]]

    def activation(act, x, out):
        if act == ACT_NONE:
            return x
        assert act == ACT_RELU, act
        nodes.append(helper.make_node("Relu", [x], [out]))
        return out

    for n in range(graph.OperatorsLength()):
        op = graph.Operators(n)
        code = model.OperatorCodes(op.OpcodeIndex())
        kind = opnames[code.BuiltinCode()]
        ins = [op.Inputs(k) for k in range(op.InputsLength())]
        outs = [op.Outputs(k) for k in range(op.OutputsLength())]
        o = name(outs[0])
        if kind == "DEQUANTIZE":
            consts[outs[0]] = const(ins[0]).astype(np.float32)
        elif kind in ("CONV_2D", "DEPTHWISE_CONV_2D"):
            opt = tflite.Conv2DOptions() if kind == "CONV_2D" else tflite.DepthwiseConv2DOptions()
            opt.Init(op.BuiltinOptions().Bytes, op.BuiltinOptions().Pos)
            assert opt.Padding() in (0, 1)  # SAME / VALID
            k = (shape(ins[1])[1], shape(ins[1])[2])
            s = (opt.StrideH(), opt.StrideW())
            pads = [0, 0, 0, 0] if opt.Padding() == 1 else same_pads(shape(ins[0])[1:3], shape(outs[0])[1:3], k, s)
            if kind == "CONV_2D":
                w, group = weight(ins[1], (0, 3, 1, 2)), 1
            else:
                assert opt.DepthMultiplier() == 1
                w = weight(ins[1], (3, 0, 1, 2))
                group = w.shape[0]
            node_out = o if opt.FusedActivationFunction() == ACT_NONE else o + "_pre"
            nodes.append(helper.make_node(
                "Conv", [name(ins[0]), init(w, f"w{n}"), init(weight(ins[2]), f"b{n}")], [node_out],
                kernel_shape=list(k), strides=list(s), pads=pads, group=group, dilations=[1, 1]))
            activation(opt.FusedActivationFunction(), node_out, o)
        elif kind in ("ADD", "MUL"):
            opt = tflite.AddOptions() if kind == "ADD" else tflite.MulOptions()
            opt.Init(op.BuiltinOptions().Bytes, op.BuiltinOptions().Pos)
            assert opt.FusedActivationFunction() == ACT_NONE
            nodes.append(helper.make_node("Add" if kind == "ADD" else "Mul", [name(ins[0]), name(ins[1])], [o]))
        elif kind == "RELU":
            nodes.append(helper.make_node("Relu", [name(ins[0])], [o]))
        elif kind == "LOGISTIC":
            nodes.append(helper.make_node("Sigmoid", [name(ins[0])], [o]))
        elif kind == "HARD_SWISH":
            # x * relu6(x + 3) / 6
            nodes.append(helper.make_node("HardSigmoid", [name(ins[0])], [o + "_hs"], alpha=1 / 6, beta=0.5))
            nodes.append(helper.make_node("Mul", [name(ins[0]), o + "_hs"], [o]))
        elif kind == "AVERAGE_POOL_2D":
            assert shape(outs[0])[1:3] == [1, 1]  # global pooling only
            nodes.append(helper.make_node("GlobalAveragePool", [name(ins[0])], [o]))
        elif kind == "RESIZE_BILINEAR":
            opt = tflite.ResizeBilinearOptions()
            opt.Init(op.BuiltinOptions().Bytes, op.BuiltinOptions().Pos)
            mode = "align_corners" if opt.AlignCorners() else ("half_pixel" if opt.HalfPixelCenters() else "asymmetric")
            scale = shape(outs[0])[1] / shape(ins[0])[1]
            nodes.append(helper.make_node(
                "Resize", [name(ins[0]), "", init(np.array([1, 1, scale, scale], np.float32), f"s{n}")], [o],
                mode="linear", coordinate_transformation_mode=mode))
        elif kind == "CUSTOM":
            assert code.CustomCode() == b"Convolution2DTransposeBias"
            # Custom options: int32 padding (1 = VALID), stride_w, stride_h.
            padding, sw, sh = np.frombuffer(bytes(op.CustomOptionsAsNumpy()), np.int32)
            assert (padding, sw, sh) == (1, 2, 2)
            w = weight(ins[1], (3, 0, 1, 2))  # OHWI -> IOHW
            nodes.append(helper.make_node(
                "ConvTranspose", [name(ins[0]), init(w, f"w{n}"), init(weight(ins[2]), f"b{n}")], [o],
                kernel_shape=[w.shape[2], w.shape[3]], strides=[2, 2]))
        else:
            raise SystemExit(f"unsupported operator {kind}")

    i, j = graph.Inputs(0), graph.Outputs(0)
    nchw = lambda s: [s[0], s[3], s[1], s[2]]
    onnx_graph = helper.make_graph(
        nodes, "selfie_segmenter",
        [helper.make_tensor_value_info(name(i), TensorProto.FLOAT, nchw(shape(i)))],
        [helper.make_tensor_value_info(name(j), TensorProto.FLOAT, nchw(shape(j)))], inits)
    # Opset 13: Resize with scales, HardSigmoid; nothing newer is needed.
    m = helper.make_model(onnx_graph, opset_imports=[helper.make_opsetid("", 13)], producer_name="fastcord-convert")
    m.ir_version = 8
    onnx.checker.check_model(m)
    onnx.save(m, dst)


if __name__ == "__main__":
    main(*sys.argv[1:3])
