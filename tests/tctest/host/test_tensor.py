import itertools
import math

import numpy as np
import tinychain as tc
import unittest

from .base import HostTest


ENDPOINT = "/transact/hypothetical"


class DenseTests(HostTest):
    def testConstant(self):
        c = 1.414
        shape = [3, 2, 1]

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Dense.constant(shape, c)
        cxt.result = tc.after(cxt.tensor[0, 0, 0].write(0), cxt.tensor)

        expected = expect_dense(tc.F64, shape, [0] + [c] * (np.product(shape) - 1))
        actual = self.host.post(ENDPOINT, cxt)

        self.assertEqual(expected, actual)

    def testSliceOnly(self):
        shape = [2, 5, 3, 3]

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Dense.arange(shape, 1, 91)
        cxt.slice = cxt.tensor[:, :, 1:-1, 1:-1]
        cxt.result = cxt.slice, cxt.slice.shape

        actual, actual_shape = self.host.post(ENDPOINT, cxt)
        expected = np.arange(1, 91).reshape([2, 5, 3, 3])[:, :, 1:-1, 1:-1]

        self.assertEqual(actual_shape, list(expected.shape))

        expected = expect_dense(tc.I64, list(expected.shape), expected.flatten())
        self.assertEqual(actual, expected)

    def testSliceAndWriteConstant(self):
        shape = [2, 5, 3, 3]

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Dense.arange(shape, 1, 91)
        cxt.result = tc.after(cxt.tensor[:, :, 1:-1, 1:-1].write(1), cxt.tensor)

        actual = self.host.post(ENDPOINT, cxt)
        expected = np.arange(1, 91).reshape([2, 5, 3, 3])
        expected[:, :, 1:-1, 1:-1] = 1
        expected = expect_dense(tc.I64, list(expected.shape), expected.flatten())
        self.assertEqual(actual, expected)

    def testSliceAndWriteTensor(self):
        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.zeros([2, 2, 5])
        cxt.small = tc.tensor.Dense.arange([2, 5], 1, 11)
        cxt.result = tc.after(cxt.big[1, :2].write(cxt.small[0]), cxt.big)

        actual = self.host.post(ENDPOINT, cxt)

        expected = np.zeros([2, 2, 5], np.int64)
        expected[1, 0:2] = np.arange(1, 11).reshape(2, 5)[0]
        expected = expect_dense(tc.F64, [2, 2, 5], expected.flatten())

        self.assertEqual(actual, expected)

    def testAdd(self):
        cxt = tc.Context()
        cxt.left = tc.tensor.Dense.arange([5, 2, 2], 1.0, 21.0)
        cxt.right = tc.tensor.Dense.constant([2, 5, 1, 2], 2)
        cxt.result = cxt.left + cxt.right

        actual = self.host.post(ENDPOINT, cxt)

        left = np.arange(1.0, 21.0, 1.0).reshape([5, 2, 2])
        right = np.ones([2, 5, 1, 2], np.int32) * 2
        expected = expect_dense(tc.F64, [2, 5, 2, 2], (left + right).flatten())

        self.assertEqual(actual, expected)

    def testCast(self):
        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.random_normal([3, 3])
        cxt.y1 = cxt.x / tc.Float(3.0)
        cxt.y2 = cxt.x / tc.Int(3).cast(tc.Float)
        cxt.y3 = cxt.x.cast(tc.F32) / 3
        cxt.result = (cxt.y1.dtype, cxt.y2.dtype, cxt.y3.dtype)

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(
            actual, tc.to_json([{tc.URI(tc.Class): {tc.URI(tc.F32): {}}}] * 3)
        )

    def testDiv(self):
        shape = [3]

        cxt = tc.Context()
        cxt.left = tc.tensor.Dense.arange(shape, 2.0, 8.0)
        cxt.right = tc.tensor.Dense.constant([1], 2)
        cxt.result = cxt.left / cxt.right

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_dense(tc.F64, shape, np.arange(1, 4))
        self.assertEqual(actual, expected)

    def testMul(self):
        shape = [5, 2, 1]

        cxt = tc.Context()
        cxt.left = tc.tensor.Dense.arange(shape, 1, 11)
        cxt.right = tc.tensor.Dense.constant([5], 2)
        cxt.result = cxt.left * cxt.right

        actual = self.host.post(ENDPOINT, cxt)

        left = np.arange(1, 11).reshape(shape)
        right = np.ones([5]) * 2
        expected = left * right
        expected = expect_dense(tc.I64, list(expected.shape), expected.flatten())
        self.assertEqual(actual, expected)

    def testNorm_matrix(self):
        shape = [2, 3, 4]
        matrices = np.arange(24).reshape(shape)
        expected = np.stack([np.linalg.norm(matrix) for matrix in matrices])

        cxt = tc.Context()
        cxt.matrices = load_dense(matrices, tc.F32)
        cxt.actual = cxt.matrices.norm()

        actual = self.host.post(ENDPOINT, cxt)
        self.assertTrue(all_close(actual, expected))

    def testNorm_column(self):
        shape = [2, 3, 4]
        matrices = np.arange(24).reshape(shape)
        expected = np.stack([np.linalg.norm(matrix, axis=-1) for matrix in matrices])

        cxt = tc.Context()
        cxt.matrices = load_dense(matrices, tc.F32)
        cxt.actual = cxt.matrices.norm(-1)

        actual = self.host.post(ENDPOINT, cxt)
        self.assertTrue(all_close(actual, expected))

    def testPow(self):
        cxt = tc.Context()
        cxt.left = tc.tensor.Dense.load([1, 2], [1, 2], tc.I64)
        cxt.result = cxt.left**2

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_dense(tc.I64, [1, 2], [1, 4])

        self.assertEqual(actual, expected)

    def testSub(self):
        shape = [1, 3]

        cxt = tc.Context()
        cxt.left = tc.tensor.Dense.arange(shape, 0, 6)
        cxt.right = tc.tensor.Dense.constant([1], 2)
        cxt.result = cxt.left - cxt.right

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_dense(tc.I64, shape, np.arange(-2, 4, 2))
        self.assertEqual(actual, expected)

    def testLogarithm(self):
        size = 1_000
        shape = [10, size / 10]

        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.arange(shape, 2, size + 2)
        cxt.ln = cxt.x.log()
        cxt.log = cxt.x.log(math.e)
        cxt.test = (cxt.ln == cxt.log).all()

        actual = self.host.post(ENDPOINT, cxt)

        self.assertEqual(actual, True)

    def testLogic(self):
        big = [20, 20, 10]
        trailing = [10]

        cxt = tc.Context()
        cxt.big_ones = tc.tensor.Dense.ones(big)
        cxt.big_zeros = tc.tensor.Dense.zeros(big)
        cxt.true = tc.tensor.Dense.ones(trailing)
        cxt.false = tc.tensor.Dense.zeros(trailing)
        cxt.result = [
            cxt.big_ones.logical_and(cxt.false).any(),
            cxt.big_ones.logical_and(cxt.true).all(),
            cxt.big_zeros.logical_or(cxt.true).all(),
            cxt.big_zeros.logical_or(cxt.false).any(),
            cxt.big_ones.logical_xor(cxt.big_zeros).all(),
        ]

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(actual, [False, True, True, False, True])

    def testMatMul(self):
        l = np.arange(12).reshape((3, 4))
        r = np.arange(20).reshape((4, 5))

        cxt = tc.Context()
        cxt.l = tc.tensor.Dense.load(l.shape, l.flatten().tolist(), tc.I32)
        cxt.r = tc.tensor.Dense.load(r.shape, r.flatten().tolist(), tc.I32)
        cxt.result = cxt.l @ cxt.r

        expected = np.matmul(l, r)

        actual = self.host.post(ENDPOINT, cxt)
        actual = actual[tc.URI(tc.tensor.Dense)][1]

        self.assertTrue(np.allclose(expected.flatten(), actual))

    def testMean(self):
        shape = [2, 3, 4]
        axis = 1
        x = np.ones(shape)

        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.load(shape, x.flatten().tolist())
        cxt.result = cxt.x.mean(axis)

        actual = self.host.post(ENDPOINT, cxt)
        expected = np.mean(x, axis)

        self.assertTrue(all_close(actual, expected))

    def testProduct_axis(self):
        shape = [2, 3, 4]
        axis = 1

        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.arange(shape, 0, 24)
        cxt.result = cxt.big.product(axis)

        actual = self.host.post(ENDPOINT, cxt)

        big = np.arange(0, 24).reshape(shape)
        expected = np.product(big, axis)

        self.assertEqual(actual, expect_dense(tc.I64, [2, 4], expected.flatten()))

    def testProduct_all(self):
        shape = [2, 3]

        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.arange(shape, 1, 7)
        cxt.result = cxt.big.product()

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(actual, np.product(range(1, 7)))

    def testRound(self):
        shape = [10, 20]
        x = (np.random.random(np.product(shape)) * 10).reshape(shape)

        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.load(shape, x.flatten().tolist())
        cxt.result = cxt.x.round()

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_dense(tc.F32, shape, x.round().astype(int).flatten())

        self.assertEqual(actual, expected)

    def testMax(self):
        shape = [2, 1, 3, 4]
        axis = 2

        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.arange(shape, 0.0, 24.0)
        cxt.result = cxt.big.max(axis)

        actual = self.host.post(ENDPOINT, cxt)
        expected = np.max(np.arange(0, 24).reshape(shape), axis)
        self.assertEqual(
            actual, expect_dense(tc.F64, expected.shape, expected.flatten())
        )

    def testSum(self):
        shape = [4, 2, 3, 5]
        axis = 2

        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.arange(shape, 0.0, 120.0)
        cxt.result = cxt.big.sum(axis)

        actual = self.host.post(ENDPOINT, cxt)
        expected = np.sum(np.arange(0, 120).reshape(shape), axis)
        self.assertEqual(actual, expect_dense(tc.F64, [4, 2, 5], expected.flatten()))

    def testSumAll(self):
        shape = [5, 2]

        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.arange(shape, 0, 10)
        cxt.result = cxt.big.sum()

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(actual, sum(range(10)))

    def testTanh(self):
        shape = [3, 4, 5]
        x = np.random.random(np.product(shape)).reshape(shape)

        cxt = tc.Context()
        cxt.x = load_dense(x)
        cxt.y = cxt.x.tanh()
        cxt.z = tc.math.operator.derivative_of(cxt.y)
        cxt.result = (cxt.y, cxt.z)

        actual_y, actual_z = self.host.post(ENDPOINT, cxt)

        expected_y = np.tanh(x)
        expected_z = 1 / (np.cosh(x) ** 2)

        self.assertTrue(all_close(actual_y, expected_y))
        self.assertTrue(all_close(actual_z, expected_z))

    def testExpandAndTranspose(self):
        input_shape = (5, 8)
        permutation = (0, 3, 1, 2)
        x = np.arange(np.product(input_shape)).reshape(input_shape)

        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.load(input_shape, x.flatten().tolist(), tc.I64)
        cxt.expanded = cxt.x.expand_dims(0).expand_dims(0).transpose(permutation)
        cxt.reshaped = cxt.x.reshape((1, 1) + input_shape).transpose(permutation)
        cxt.result = [cxt.reshaped, cxt.expanded]

        reshaped, expanded = self.host.post(ENDPOINT, cxt)

        expected = np.transpose(x.reshape((1, 1) + input_shape), permutation)
        expected = expected.flatten().tolist()
        expected = expect_dense(tc.I64, (1, 8, 1, 5), expected)

        self.assertEqual(reshaped, expected)
        self.assertEqual(expanded, expected)

    def testSliceAndTransposeAndSlice(self):
        self.maxDiff = None
        shape = [2, 3, 4, 5]

        cxt = tc.Context()
        cxt.big = tc.tensor.Dense.arange(shape, 0, 120)
        cxt.medium = cxt.big[0]
        cxt.small = cxt.medium.transpose()[1, 1:3]

        expected = np.arange(0, 120).reshape(shape)
        expected = expected[0]
        expected = np.transpose(expected)[1, 1:3]

        actual = self.host.post(ENDPOINT, cxt)

        expected = expect_dense(tc.I64, expected.shape, expected.flatten())
        self.assertEqual(actual, expected)

    def testReshape(self):
        source = [2, 3, 4, 1]
        dest = [3, 8]

        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.arange(source, 0, 24)
        cxt.result = cxt.x.reshape(dest)

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(actual, expect_dense(tc.I64, dest, np.arange(24).tolist()))

    def testCond(self):
        size = 5
        x = np.random.random(size).astype(bool)
        a = np.random.random(size)
        b = np.random.random(size)
        expected = np.where(x, a, b)

        cxt = tc.Context()
        cxt.x = load_dense(x, tc.Bool)
        cxt.a = load_dense(a)
        cxt.b = load_dense(b)
        cxt.result = cxt.x.cond(cxt.a, cxt.b)

        actual = self.host.post(ENDPOINT, cxt)
        self.assertTrue(all_close(actual, expected))

    def testRandomUniform(self):
        minval = -1
        maxval = 3

        cxt = tc.Context()
        cxt.x = tc.tensor.Dense.random_uniform([5, 1], minval, maxval)
        cxt.result = (
            (cxt.x >= -1)
            .all()
            .logical_and((cxt.x <= 3).all())
            .logical_and(cxt.x.mean() > 0)
        )

        actual = self.host.post(ENDPOINT, cxt)
        self.assertTrue(actual)


class SparseTests(HostTest):
    def testCreate(self):
        shape = [2, 5]
        coord = [0, 0]
        value = 1

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Sparse.zeros(shape, tc.I32)
        cxt.result = tc.after(cxt.tensor[coord].write(value), cxt.tensor)

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_sparse(tc.I32, shape, [[coord, value]])
        self.assertEqual(actual, expected)

    def testWriteAndSlice(self):
        shape = [2, 5]

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Sparse.zeros(shape)
        cxt.result = tc.after(cxt.tensor[0, 1].write(1), cxt.tensor)

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_sparse(tc.F32, shape, [[[0, 1], 1]])
        self.assertEqual(actual, expected)

    def testAdd(self):
        shape = [2, 3]

        cxt = tc.Context()
        cxt.big = tc.tensor.Sparse.zeros(shape)
        cxt.small = tc.tensor.Sparse.zeros([3])
        cxt.result = tc.after(
            [
                cxt.big[1, 1].write(1),
                cxt.small[1].write(2),
            ],
            cxt.big + cxt.small,
        )

        actual = self.host.post(ENDPOINT, cxt)

        big = np.zeros(shape)
        big[1, 1] = 1
        small = np.zeros([3])
        small[1] = 2
        expected = big + small

        expected = expect_sparse(tc.F32, shape, expected)

        self.assertEqual(actual, expected)

    def testMul(self):
        shape = (3, 2)

        data = [
            ((0, 1), 1),
            ((1, 0), 1),
            ((2, 0), 2),
            ((2, 1), 3),
        ]

        cxt = tc.Context()
        cxt.sparse = tc.tensor.Sparse.load(shape, data, tc.I32)
        cxt.result = cxt.sparse.expand_dims(2) * cxt.sparse.expand_dims(1)

        actual = self.host.post(ENDPOINT, cxt)

        expected = np.zeros(shape)
        for coord, value in data:
            expected[coord] = value
        expected = np.expand_dims(expected, 2) * np.expand_dims(expected, 1)
        expected = expect_sparse(tc.I32, expected.shape, expected)

        self.assertEqual(expected, actual)

    def testSub(self):
        shape = [3, 5, 2]

        cxt = tc.Context()
        cxt.big = tc.tensor.Sparse.zeros(shape, tc.I16)
        cxt.small = tc.tensor.Sparse.zeros([5, 2], tc.U32)
        cxt.result = tc.after(
            [
                cxt.big[1, 2, 0].write(2),
                cxt.small[3, 1].write(3),
            ],
            cxt.small - cxt.big,
        )

        actual = self.host.post(ENDPOINT, cxt)

        big = np.zeros(shape)
        big[1, 2, 0] = 2
        small = np.zeros([5, 2])
        small[3, 1] = 3
        expected = small - big

        expected = expect_sparse(tc.I16, shape, expected)
        self.assertEqual(actual, expected)

    def testSum_axis1(self):
        shape = [2, 4, 3, 5]
        axis = 1

        cxt = tc.Context()
        cxt.big = tc.tensor.Sparse.zeros(shape, tc.I32)
        cxt.result = tc.after(cxt.big[0, 1, 2, 3].write(2), cxt.big.sum(axis))

        actual = self.host.post(ENDPOINT, cxt)
        expected = np.zeros(shape, dtype=np.int32)
        expected[0, 1, 2, 3] = 2
        expected = expected.sum(axis)
        expected = expect_sparse(tc.I32, [2, 3, 5], expected)
        self.assertEqual(actual, expected)

    def testSum_axis0(self):
        shape = (3, 2)
        data = [
            ((0, 0), 1),
            ((0, 1), 1),
            ((1, 0), 2),
            ((1, 1), 2),
            ((2, 0), 3),
            ((2, 1), 3),
        ]

        cxt = tc.Context()
        cxt.sparse = tc.tensor.Sparse.load(shape, data, tc.I32)
        cxt.result = cxt.sparse.sum(0)

        actual = self.host.post(ENDPOINT, cxt)

        expected = np.zeros(shape)
        for coord, value in data:
            expected[coord] = value
        expected = np.sum(expected, 0)
        expected = expect_sparse(tc.I32, (2,), expected)

        self.assertEqual(actual, expected)

    def testProduct(self):
        shape = [2, 4, 3, 5]
        axis = 2

        cxt = tc.Context()
        cxt.big = tc.tensor.Sparse.zeros(shape, tc.I32)
        cxt.result = tc.after(cxt.big[0, 1, 2, 3].write(2), cxt.big.product(axis))

        actual = self.host.post(ENDPOINT, cxt)

        expected = np.zeros(shape, dtype=np.int32)
        expected[0, 1, 2, 3] = 2
        expected = expected.prod(axis)
        expected = expect_sparse(tc.I32, [2, 4, 5], expected)

        self.assertEqual(actual, expected)

    def testTranspose(self):
        shape = [2, 3]
        elements = [((0, 1), 1), ((0, 2), 3), ((1, 1), 2)]

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Sparse.load(shape, elements, tc.I32)
        cxt.result = cxt.tensor.transpose()

        actual = self.host.post(ENDPOINT, cxt)

        expected = np.zeros(shape)
        for coord, value in elements:
            expected[coord] = value
        expected = expect_sparse(tc.I32, reversed(shape), np.transpose(expected))

        self.assertEqual(actual, expected)

    def testBroadcastAndSlice(self):
        self.maxDiff = None

        data = [
            [[0, 1], 1.0],
            [[0, 2], 2.0],
            [[1, 0], 3.0],
        ]

        shape = [1, 4, 2, 3]

        cxt = tc.Context()
        cxt.small = tc.tensor.Sparse.load([2, 3], data, tc.I32)
        cxt.big = cxt.small * tc.tensor.Dense.ones(shape)
        cxt.slice = cxt.big[0]

        actual = self.host.post(ENDPOINT, cxt)

        expected = np.zeros([2, 3])
        for coord, value in data:
            expected[tuple(coord)] = value
        expected = expected * np.ones(shape)
        expected = expected[0]
        expected = expect_sparse(tc.F64, expected.shape, expected)

        self.assertEqual(actual, expected)

    @unittest.skip("TODO: re-enable after fixing range selection in b_table")
    def testExpandAndTransposeAndSum(self):
        shape = [2, 3]
        elements = [((0, 1), 1), ((1, 1), 2)]

        cxt = tc.Context()
        cxt.tensor = tc.tensor.Sparse.load(shape, elements, tc.I32)
        cxt.result = cxt.tensor.expand_dims(1).transpose().sum(1)

        actual = self.host.post(ENDPOINT, cxt)
        print("actual", actual)

        expected = np.zeros(shape)
        for coord, value in elements:
            expected[coord] = value

        expected = np.sum(np.transpose(np.expand_dims(expected, 1)), 1)
        expected = expect_sparse(tc.I32, expected.shape, expected)
        print("expected", expected)

        self.assertEqual(actual, expected)


class TensorTests(HostTest):
    def testAdd(self):
        cxt = tc.Context()
        cxt.dense = tc.tensor.Dense.arange([3, 5, 2], 0, 30)
        cxt.sparse = tc.tensor.Sparse.zeros([5, 2], tc.I32)
        cxt.result = tc.after(cxt.sparse[1, 1].write(3), cxt.dense + cxt.sparse)

        actual = self.host.post(ENDPOINT, cxt)
        l = np.arange(0, 30).reshape([3, 5, 2])
        r = np.zeros([5, 2], np.int32)
        r[1, 1] = 3
        expected = l + r
        self.assertEqual(actual, expect_dense(tc.I64, [3, 5, 2], expected.flatten()))

    def testDiv(self):
        self.maxDiff = None
        cxt = tc.Context()
        cxt.dense = tc.tensor.Dense.arange([30, 3, 2], 1.0, 181.0)
        cxt.sparse = tc.tensor.Sparse.zeros([3, 2], tc.F32)
        cxt.result = tc.after(cxt.sparse[1, 0].write(2), cxt.sparse / cxt.dense)

        actual = self.host.post(ENDPOINT, cxt)
        l = np.arange(1, 181).reshape([30, 3, 2])
        r = np.zeros([3, 2], float)
        r[1, 0] = 2.0
        expected = r / l
        self.assertEqual(actual, expect_sparse(tc.F64, [30, 3, 2], expected))

    def testMul(self):
        cxt = tc.Context()
        cxt.dense = tc.tensor.Dense.arange([3], 0, 3)
        cxt.sparse = tc.tensor.Sparse.zeros([2, 3], tc.I32)
        cxt.result = tc.after(cxt.sparse[0, 1].write(2), cxt.dense * cxt.sparse)

        actual = self.host.post(ENDPOINT, cxt)
        expected = np.zeros([2, 3])
        expected[0, 1] = 2
        expected = expected * np.arange(0, 3)
        self.assertEqual(actual, expect_sparse(tc.I64, [2, 3], expected))

    def testSubAndSum(self):
        x = 300
        y = 250
        z = 2

        cxt = tc.Context()
        cxt.sparse = tc.tensor.Sparse.zeros([1, y, z])
        cxt.dense = tc.tensor.Dense.ones([x, 1, z])
        cxt.result = (cxt.sparse - cxt.dense).sum()

        actual = self.host.post(ENDPOINT, cxt)
        self.assertEqual(actual, -(x * y * z))

    def testSparseAsDense(self):
        matrix = np.eye(3).astype(bool)
        data = [
            (list(coord), bool(matrix[coord]))
            for coord in np.ndindex(matrix.shape)
            if matrix[coord] != 0
        ]

        cxt = tc.Context()
        cxt.sparse = tc.tensor.Sparse.load([3, 3], data, tc.Bool)
        cxt.dense = cxt.sparse.as_dense()

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_dense(tc.Bool, [3, 3], matrix.flatten().tolist())
        self.assertEqual(actual, expected)

    def testDenseAsSparse(self):
        matrix = np.eye(3).astype(int)

        cxt = tc.Context()
        cxt.dense = load_dense(matrix, tc.I32)
        cxt.sparse = cxt.dense.as_sparse()

        actual = self.host.post(ENDPOINT, cxt)
        expected = expect_sparse(tc.I32, [3, 3], matrix)
        self.assertEqual(actual, expected)

    def testConcatenate(self):
        x1 = np.ones([5, 8], int)
        x2 = np.ones([5, 4], int) * 2

        cxt = tc.Context()
        cxt.x1 = load_dense(x1, tc.I32)
        cxt.x2 = load_dense(x2, tc.I32)
        cxt.result = tc.tensor.Dense.concatenate([cxt.x1, cxt.x2], axis=1)
        actual = self.host.post(ENDPOINT, cxt)

        expected = np.concatenate([x1, x2], axis=1)
        self.assertEqual(
            actual, expect_dense(tc.I32, [5, 12], expected.flatten().tolist())
        )


def all_close(actual, expected):
    return np.allclose(actual[tc.URI(tc.tensor.Dense)][1], expected.flatten())


def expect_dense(dtype, shape, flat):
    return {
        str(tc.URI(tc.tensor.Dense)): [
            [str(tc.URI(dtype)), list(shape)],
            list(flat),
        ]
    }


def expect_sparse(dtype, shape, values):
    if isinstance(values, np.ndarray):
        values = nparray_to_sparse(values, dtype)

    return {
        str(tc.URI(tc.tensor.Sparse)): [
            [str(tc.URI(dtype)), list(shape)],
            list(values),
        ]
    }


def load_dense(x, dtype=tc.F32):
    return tc.tensor.Dense.load(x.shape, x.flatten().tolist(), dtype)


def nparray_to_sparse(arr, dtype):
    dtype = float if issubclass(dtype, tc.Float) else int
    zero = dtype(0)
    coords = itertools.product(*[range(dim) for dim in arr.shape])
    sparse = [
        [list(coord), n]
        for (coord, n) in zip(coords, (dtype(n) for n in arr.flatten()))
        if n != zero
    ]
    return sparse


if __name__ == "__main__":
    unittest.main()
