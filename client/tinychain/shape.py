import functools
import operator
import typing

from .generic import Tuple
from .scalar.bound import Range
from .scalar.number import Number, U64
from .scalar.ref import form_of, is_literal, get_ref
from .state import State
from .util import uri


class Shape(Tuple):
    __spec__ = typing.Tuple[U64, ...]

    def __new__(cls, form):
        if hasattr(form, "__iter__"):
            spec = tuple(U64 for _ in form)
            return State.__new__(cls.expect(spec))
        else:
            return State.__new__(cls)

    def __add__(self, other):
        if hasattr(self, "__len__") and hasattr(other, "__len__"):
            return Shape(tuple(form_of(self)) + tuple(form_of(other)))
        else:
            raise ValueError(f"can only append Shapes with a constant number of dimensions, not {self} and {other}")

    def __getitem__(self, x):
        if x is None:
            raise ValueError(f"invalid axis: {x}")

        spec = typing.get_args(self.__spec__) if typing.get_args(self.__spec__) else self.__spec__

        if isinstance(x, Range):
            x = x.to_slice()

        if isinstance(x, slice):
            start = form_of(x.start)
            stop = form_of(x.stop)

            if x.step is not None:
                raise NotImplementedError(f"slice with step: {x}")

            if len(spec) != 2 or (len(spec) == 2 and spec[1] is not Ellipsis):
                # the contents are constants, so compute the slice now if possible
                if hasattr(form_of(self), "__getitem__"):
                    if is_literal(start) and is_literal(stop):
                        start = _index_of(start, len(self), 0)
                        stop = _index_of(stop, len(self), len(self))
                        return Shape([self[i] for i in range(start, stop)])

            return self._get("", Range.from_slice(x), Shape)

        if is_literal(x):
            x = form_of(x)
            if hasattr(form_of(self), "__getitem__"):
                item = form_of(self)[x]
                if uri(self) == uri(self.__class__):
                    return item
                else:
                    return get_ref(item, uri(self).append(x))

        return self._get("", x, U64)

    @classmethod
    def concatenate(cls, shapes, axis=0):
        if not shapes:
            raise ValueError("cannot concatenate an empty list of shapes")
        elif not hasattr(shapes, "__len__"):
            raise ValueError(f"can only concatenate a constant list of shapes, not {shapes}")

        ndim = shapes[0].ndim(True, "concatenate")
        for shape in shapes:
            if shape.ndim(True, "concatenate") != ndim:
                raise ValueError("shapes to concatenate must have the same number of dimensions")

        if isinstance(form_of(axis), int):
            axis = form_of(axis)
            axis = ndim + axis if axis < 0 else axis
        else:
            raise ValueError(f"Shape.concatenate requires a constant axis, not {axis}")

        dim = 0
        for shape in shapes:
            if shape[axis] is None:
                raise ValueError(f"dimension for concatenation at axis {axis} is not constant: {shape[axis]}")

            if is_literal(shape[axis]):
                dim += form_of(shape[axis])
            else:
                raise ValueError(f"Shape.concatenate requires constant dimensions along the axis {axis}")

        concatenated = [None] * ndim
        concatenated[axis] = dim

        for x in (x for x in range(ndim) if x != axis):
            for shape in shapes:
                if concatenated[x] is None:
                    concatenated[x] = shape[x]
                elif concatenated[x] != shape[x]:
                    raise ValueError(f"cannot concatenate {shapes} due to inconsistent dimension at axis {x}")

            if concatenated[x] is None:
                raise ValueError(f"shape of concatenated tensor is not constant at axis {x}")

        return Shape(concatenated)

    def ndim(self, require_constant=False, op_name="this operation"):
        if require_constant and not hasattr(self, "__len__"):
            raise RuntimeError(f"to {op_name} {self} requires a constant number of dimensions")

        if hasattr(self, "__len__"):
            return len(self)
        else:
            return self.len()

    def broadcast(self, other):
        ndim = max(self.ndim(True, "broadcast"), other.ndim(True, "broadcast"))

        assert int(ndim) == ndim

        if not ndim:
            return Shape(tuple())

        shape = [1] * ndim

        left = [1] * (ndim - self.ndim()) + list(self)
        right = [1] * (ndim - other.ndim()) + list(other)
        assert len(left) == len(right)

        for x, (l, r) in reversed(list(enumerate(zip(left, right)))):
            if is_literal(l) and is_literal(r):
                if l == r:
                    dim = l
                elif l == 1:
                    dim = r
                elif r == 1:
                    dim = l
                else:
                    raise ValueError(f"cannot broadcast dimensions {l} and {r}")

            elif is_literal(l):
                if l == 1:
                    dim = r
                else:
                    dim = l  # assume l == r

            elif is_literal(r):
                if r == 1:
                    dim = l
                else:
                    dim = r  # assume l == r

            else:
                raise ValueError(f"broadcast requires at least one of the dimensions {l} and {r} to be constant")

            shape[x] = dim

        return Shape(shape)

    def expand(self, axis=None):
        if not hasattr(self, "__len__"):
            raise RuntimeError(f"Shape.expand requires a constant number of dimensions")

        if not is_literal(axis):
            raise ValueError(f"Shape.expand requires a constant axis, not {axis}")

        if axis is None:
            return Shape(self + [1])
        else:
            return Shape(self[:axis] + [1] + self[axis:])

    def reduce(self, axis=None, keepdims=False):
        if not is_literal(axis):
            raise ValueError(f"Shape.reduce requires a constant axis, not {axis}")

        if not is_literal(keepdims):
            return ValueError(f"the keepdims parameter of Shape.reduce must be a constant, not {keepdims}")

        if not keepdims and axis is None:
            return Shape(tuple())

        ndim = self.ndim(True, "reduce")
        axis = ndim + axis if axis < 0 else axis
        if keepdims:
            return Shape(tuple(1 if x == axis else dim for x, dim in enumerate(self)))
        else:
            return Shape(tuple(dim for x, dim in enumerate(self) if x != axis))

    def reshape(self, new_shape):
        if is_literal(new_shape):
            new_shape = form_of(new_shape)
        else:
            return new_shape

        for (x, dim) in enumerate(new_shape):
            if dim is not None and dim < 0:
                raise ValueError(f"invalid dimension for reshape at axis {x}: {dim}")

        if len([dim for dim in new_shape if dim is None]) > 1:
            raise ValueError(f"Shape.reshape supports a maximum of one unknown dimension, not {new_shape}")

        if is_literal(self):
            this_size = functools.reduce(operator.mul, form_of(self))
            for x in range(len(new_shape)):
                if new_shape[x] is None:
                    that_size = functools.reduce(operator.mul, new_shape[:x] + new_shape[x + 1:])
                    new_shape[x] = this_size / that_size

            that_size = functools.reduce(operator.mul, (dim for dim in new_shape if dim is not None))
            if this_size != that_size:
                raise ValueError(f"cannot reshape {self} into {new_shape}")

            return Shape(new_shape)
        elif any(dim is None for dim in new_shape):
            raise ValueError(f"non-constant {self} does not support reshape with an unknown dimension: {new_shape}")
        else:
            return Shape(new_shape)

    def slice(self, bounds):
        if not hasattr(bounds, "__iter__"):
            raise ValueError(f"the shape of a Tensor slice requires constant-length bounds, not {bounds}")

        shape = []
        for x, bound in enumerate(bounds):
            if isinstance(bound, Range):
                bound = bound.to_slice()

            if isinstance(bound, slice):
                start = 0 if bound.start is None else form_of(bound.start)
                stop = form_of(self[x]) if bound.stop is None else form_of(bound.stop)
                if not is_literal((start, stop)):
                    raise ValueError(f"the shape of a Tensor slice requires a constant bound, not {(start, stop)}")

                if start < 0 or stop < 0:
                    if is_literal(self[x]):
                        dim = self[x]
                    else:
                        raise RuntimeError(f"Shape.slice requires a constant dimension for axis {x}, not {self[x]}")

                    start = dim + start if start < 0 else start
                    stop = dim + stop if stop < 0 else stop

                shape.append(stop - start)
            elif isinstance(bound, (int, Number)):
                # no-op -- dimension elided
                pass
            else:
                raise ValueError(f"invalid axis bound: {bound}")

        for x in range(len(bounds), self.ndim(True, "slice")):
            shape.append(self[x])

        return Shape(shape)

    def transpose(self, permutation=None):
        if permutation is None:
            if not hasattr(self, "__reversed__"):
                raise RuntimeError(f"Shape.transpose requires a constant-length shape, not {self}")

            return reversed(self)
        elif is_literal(permutation):
            return Shape(tuple(self[x] for x in permutation))
        else:
            raise ValueError(f"Shape.transpose requires a constant permutation, not {permutation}")


def _index_of(i, length, default):
    if i is None:
        idx = default
    elif i < 0:
        idx = length + i
    else:
        idx = i

    return idx
