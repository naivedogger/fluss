/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *    http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package com.alibaba.fluss.row.arrow.vectors;

import com.alibaba.fluss.annotation.Internal;
import com.alibaba.fluss.row.columnar.BooleanColumnVector;
import com.alibaba.fluss.shaded.arrow.org.apache.arrow.vector.BitVector;

import static com.alibaba.fluss.utils.Preconditions.checkNotNull;

/** Arrow column vector for Boolean. */
@Internal
public class ArrowBooleanColumnVector implements BooleanColumnVector {

    /** Container which is used to store the sequence of boolean values of a column to read. */
    private final BitVector bitVector;

    public ArrowBooleanColumnVector(BitVector bitVector) {
        this.bitVector = checkNotNull(bitVector);
    }

    @Override
    public boolean getBoolean(int i) {
        return bitVector.get(i) != 0;
    }

    @Override
    public boolean isNullAt(int i) {
        return bitVector.isNull(i);
    }
}
